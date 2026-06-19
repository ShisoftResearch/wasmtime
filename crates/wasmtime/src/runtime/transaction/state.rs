use crate::runtime::transaction::concurrency::TransactionConflictAction;

use super::*;

#[derive(Debug)]
pub(crate) struct TransactionState {
    pub(super) active: Option<TransactionId>,
    pub(super) failed: bool,
    pub(super) failure_code: u32,
    pub(super) fail_next_commit_before_lp_for_test: bool,
    pub(super) persistent_gc_state: Option<PersistentGcState>,
    pub(super) persistent_roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
    pub(super) persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    pub(super) next_id: u64,
    pub(super) shared_region_runtime: Option<TransactionRegionRuntime>,
    pub(super) terminal_commit_active: bool,
    pub(super) active_conflict_aborted: bool,
    pub(super) concurrency: ConcurrencyControlState,
    pub(super) suspended: BTreeMap<TransactionId, TransactionWorkspace>,
    pub(super) staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    pub(super) staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    pub(super) staged_memory_sizes: BTreeMap<GranuleId, u64>,
    pub(super) staged_table_sizes: BTreeMap<GranuleId, u64>,
    pub(super) staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    pub(super) staged_objects: BTreeMap<ObjectId, ObjectPayload>,
    pub(super) pending_persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    pub(super) promoted_objects: BTreeMap<ObjectId, ObjectId>,
    pub(super) promoted_gc_refs: BTreeMap<u32, ObjectId>,
    pub(super) durable_leaf_gc_refs: BTreeSet<u32>,
    pub(super) allocated_objects: Vec<ObjectId>,
    pub(super) pending_conflict_aborted_allocated_objects: Vec<ObjectId>,
    pub(super) granule_versions: BTreeMap<GranuleId, u64>,
    pub(super) read_granules: BTreeSet<GranuleId>,
    pub(super) write_granules: BTreeSet<GranuleId>,
    pub(super) uncommitted_publication_streams: BTreeSet<u32>,
    pub(super) pending_linear_undo_chunks: BTreeMap<u32, BTreeSet<u32>>,
    pub(super) post_commit_linear_undo_chunks: BTreeMap<u32, BTreeSet<u32>>,
    pub(super) scratch: Vec<u8>,
    pub(super) pending_memory_store: Option<PendingMemoryStore>,
    pub(super) durable_log: TxDurableLog,
}

#[derive(Debug, Default)]
pub(super) struct TransactionWorkspace {
    staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    staged_memory_sizes: BTreeMap<GranuleId, u64>,
    staged_table_sizes: BTreeMap<GranuleId, u64>,
    staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    staged_objects: BTreeMap<ObjectId, ObjectPayload>,
    pending_persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    promoted_objects: BTreeMap<ObjectId, ObjectId>,
    promoted_gc_refs: BTreeMap<u32, ObjectId>,
    durable_leaf_gc_refs: BTreeSet<u32>,
    allocated_objects: Vec<ObjectId>,
    conflict_aborted: bool,
    read_granules: BTreeSet<GranuleId>,
    write_granules: BTreeSet<GranuleId>,
    uncommitted_publication_streams: BTreeSet<u32>,
    pending_linear_undo_chunks: BTreeMap<u32, BTreeSet<u32>>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            failed: false,
            failure_code: 0,
            fail_next_commit_before_lp_for_test: false,
            persistent_gc_state: None,
            persistent_roots: BTreeMap::new(),
            persistent_root_versions: BTreeMap::new(),
            next_id: 10_001,
            shared_region_runtime: None,
            terminal_commit_active: false,
            active_conflict_aborted: false,
            concurrency: ConcurrencyControlState::default(),
            suspended: BTreeMap::new(),
            staged_globals: BTreeMap::new(),
            staged_granules: BTreeMap::new(),
            staged_memory_sizes: BTreeMap::new(),
            staged_table_sizes: BTreeMap::new(),
            staged_table_elements: BTreeMap::new(),
            staged_objects: BTreeMap::new(),
            pending_persistent_root_versions: BTreeMap::new(),
            promoted_objects: BTreeMap::new(),
            promoted_gc_refs: BTreeMap::new(),
            durable_leaf_gc_refs: BTreeSet::new(),
            allocated_objects: Vec::new(),
            pending_conflict_aborted_allocated_objects: Vec::new(),
            granule_versions: BTreeMap::new(),
            read_granules: BTreeSet::new(),
            write_granules: BTreeSet::new(),
            uncommitted_publication_streams: BTreeSet::new(),
            pending_linear_undo_chunks: BTreeMap::new(),
            post_commit_linear_undo_chunks: BTreeMap::new(),
            scratch: Vec::new(),
            pending_memory_store: None,
            durable_log: TxDurableLog::default(),
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
    TableSize {
        owner_instance: Option<InstanceId>,
        table_index: u32,
        new_elements: u64,
    },
    TableElement {
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        value: TableElementSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalSnapshot {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
    /// SHISOFT-TWASM-MOCK: live reference snapshots still use raw Wasmtime
    /// reference words at some helper boundaries. Durable object/root records
    /// use `ObjectId`; final live `tref` lowering must remove this bridge.
    GcRef(u32),
    FuncRef(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TableElementSnapshot {
    /// Raw `VMFuncRef` pointer address, with `0` representing null.
    FuncRef(usize),
    /// Raw nullable `VMGcRef` word used at live table helper boundaries.
    /// Durable roots use `ObjectId`; final live `tref` lowering must remove
    /// this bridge.
    GcRef(u32),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum PersistentRootKey {
    Global {
        instance: Option<u32>,
        global_index: u32,
    },
    TableElement(TableElementKey),
    Recovered,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentRootDelta {
    pub(super) roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
}

impl PersistentRootDelta {
    pub(super) fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

impl TransactionState {
    pub(crate) fn begin(&mut self) -> Result<TransactionId> {
        self.prepare_to_begin_transaction()?;
        let id = self.allocate_local_transaction_id()?;
        self.activate_transaction(id);
        Ok(id)
    }

    pub(crate) fn begin_with_region_runtime(
        &mut self,
        region: &TransactionRegionRuntime,
    ) -> Result<TransactionId> {
        self.prepare_to_begin_transaction()?;
        let id = region.allocate_transaction_id()?;
        self.shared_region_runtime = Some(region.clone());
        self.activate_transaction(id);
        Ok(id)
    }

    fn prepare_to_begin_transaction(&mut self) -> Result<()> {
        self.retry_post_commit_linear_undo_retirement();
        ensure!(
            !self.failed,
            "cannot begin transaction while structured ttry failure is pending"
        );
        ensure!(
            self.active.is_none(),
            "transaction is already active in this store"
        );
        Ok(())
    }

    fn allocate_local_transaction_id(&mut self) -> Result<TransactionId> {
        let id = TransactionId::from_raw(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("transaction id overflow")?;
        Ok(id)
    }

    fn activate_transaction(&mut self, id: TransactionId) {
        self.active = Some(id);
        replace_current_thread_transaction(Some(id));
    }

    pub(crate) fn enter_transaction(
        &mut self,
        transaction: TransactionId,
    ) -> Result<Option<TransactionId>> {
        self.retry_post_commit_linear_undo_retirement();
        ensure!(
            !self.failed,
            "cannot enter transaction while structured ttry failure is pending"
        );
        ensure!(
            self.active != Some(transaction),
            "transaction id is already current"
        );
        let previous = self.suspend_active_workspace()?;
        let workspace = self.suspended.remove(&transaction).unwrap_or_default();
        self.install_workspace(workspace);
        self.active = Some(transaction);
        replace_current_thread_transaction(Some(transaction));
        Ok(previous)
    }

    pub(crate) fn restore_transaction(&mut self, previous: Option<TransactionId>) -> Result<()> {
        self.retry_post_commit_linear_undo_retirement();
        self.suspend_active_workspace()?;
        match previous {
            Some(transaction) => {
                let workspace = self
                    .suspended
                    .remove(&transaction)
                    .with_context(|| format!("transaction is not suspended: {transaction:?}"))?;
                self.install_workspace(workspace);
                self.active = Some(transaction);
            }
            None => {
                self.install_workspace(TransactionWorkspace::default());
                self.active = None;
            }
        }
        replace_current_thread_transaction(previous);
        Ok(())
    }

    pub(crate) fn transaction_is_open(&self, transaction: TransactionId) -> bool {
        self.active == Some(transaction) || self.suspended.contains_key(&transaction)
    }

    pub(crate) fn abort_transaction(&mut self, transaction: TransactionId) -> Result<bool> {
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        if self.active == Some(transaction) {
            self.abort()?;
            return Ok(true);
        }
        self.retry_post_commit_linear_undo_retirement();
        if let Some(workspace) = self.suspended.get(&transaction) {
            ensure!(
                workspace.allocated_objects.is_empty(),
                "generic transaction abort requires object-aware cleanup for allocated objects"
            );
        }
        if let Some(workspace) = self.suspended.remove(&transaction) {
            self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
            self.release_transaction_authority(transaction)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn abort_transaction_allocated_objects(
        &mut self,
        object_table: &mut ObjectTable,
        transaction: TransactionId,
    ) -> Result<bool> {
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        if self.active == Some(transaction) {
            self.abort_allocated_objects(object_table)?;
            return Ok(true);
        }
        self.retry_post_commit_linear_undo_retirement();
        let Some(workspace) = self.suspended.remove(&transaction) else {
            return Ok(false);
        };
        for object_id in workspace.allocated_objects.iter().rev().copied() {
            object_table.free(object_id)?;
        }
        self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
        self.release_transaction_authority(transaction)?;
        Ok(true)
    }

    pub(crate) fn active_transaction(&self) -> Option<TransactionId> {
        self.active
    }

    pub(crate) fn fail_next_commit_before_lp_for_test(&mut self) {
        self.fail_next_commit_before_lp_for_test = true;
    }

    pub(crate) fn take_fail_next_commit_before_lp_for_test(&mut self) -> bool {
        mem::take(&mut self.fail_next_commit_before_lp_for_test)
    }

    pub(crate) fn active_transaction_required_raw(&self) -> Result<u64> {
        Ok(self.active_transaction_required()?.as_raw())
    }

    pub(crate) fn shared_region_runtime_for_publication(
        &self,
    ) -> Option<&TransactionRegionRuntime> {
        self.shared_region_runtime.as_ref()
    }

    pub(crate) fn set_shared_region_runtime(&mut self, runtime: Option<TransactionRegionRuntime>) {
        self.shared_region_runtime = runtime;
    }

    pub(crate) fn create_file_backed_durable_log(
        &mut self,
        path: &Path,
        num_blocks: u32,
    ) -> Result<()> {
        self.durable_log = TxDurableLog::create_file_backed(path, num_blocks)?;
        Ok(())
    }

    pub(crate) fn create_shared_file_backed_durable_log(
        &mut self,
        shared: &crate::runtime::transaction::region_runtime::SharedFileBackedStorageConfig,
    ) -> Result<()> {
        self.durable_log = TxDurableLog::create_file_backed_with_allocator_lock(
            shared.tx_log_path(),
            shared.tx_log_blocks(),
            shared.durable_log_allocator_lock(),
        )?;
        Ok(())
    }

    pub(crate) fn open_file_backed_durable_log(&mut self, path: &Path) -> Result<()> {
        self.durable_log = TxDurableLog::open_file_backed(path)?;
        Ok(())
    }

    pub(crate) fn open_shared_file_backed_durable_log(
        &mut self,
        shared: &crate::runtime::transaction::region_runtime::SharedFileBackedStorageConfig,
    ) -> Result<()> {
        self.durable_log = TxDurableLog::open_file_backed_with_allocator_lock(
            shared.tx_log_path(),
            shared.durable_log_allocator_lock(),
        )?;
        Ok(())
    }

    pub(crate) fn publish_tmemory_undo_before_in_place_write(
        &mut self,
        stream_id: u32,
        txid: u32,
        undo: &PendingGranuleUndo,
    ) -> Result<PendingCommitLogEntry> {
        self.uncommitted_publication_streams.insert(stream_id);
        let mut sink = self.durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        let marker = publisher.publish_tmemory_undo_before_in_place_write(undo)?;
        self.pending_linear_undo_chunks
            .entry(stream_id)
            .or_default()
            .insert(marker.chunk_start_block);
        Ok(marker)
    }

    pub(crate) fn publish_object_publications_before_commit(
        &mut self,
        stream_id: u32,
        txid: u32,
        object_table: &ObjectTable,
        publications: &[persist::PendingPublication],
    ) -> Result<Option<PendingCommitLogEntry>> {
        if publications.is_empty() {
            return Ok(None);
        }
        self.uncommitted_publication_streams.insert(stream_id);
        let mut required_layout_ids = BTreeSet::new();
        let mut required_layouts = Vec::new();
        for publication in publications {
            let Some(type_layout_id) = publication.persistent_object_type_layout_id()? else {
                continue;
            };
            if required_layout_ids.insert(type_layout_id) {
                required_layouts.push(object_table.require_type_layout(type_layout_id)?);
            }
        }
        for layout in required_layouts {
            self.durable_log.ensure_type_layout(layout)?;
        }

        let mut sink = self.durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        publisher.publish_object_publications_before_commit(publications)
    }

    pub(crate) fn publish_commit_lp(
        &mut self,
        stream_id: u32,
        txid: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        {
            let mut sink = self.durable_log.stream_sink(stream_id);
            let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
            publisher.publish_commit_lp(marker)?;
        }
        self.uncommitted_publication_streams.remove(&stream_id);
        if let Some(chunk_starts) = self.pending_linear_undo_chunks.remove(&stream_id) {
            self.post_commit_linear_undo_chunks
                .entry(stream_id)
                .or_default()
                .extend(chunk_starts);
        }
        self.retry_post_commit_linear_undo_retirement_for_stream(stream_id);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn durable_log_entries_for_test(
        &self,
        stream_id: u32,
    ) -> Vec<crate::vm::TxLogEntry> {
        self.durable_log.log_entries_for_test(stream_id)
    }

    pub(crate) fn structured_failure_pending(&self) -> bool {
        self.failed
    }

    pub(crate) fn structured_failure_code(&self) -> u32 {
        self.failure_code
    }

    pub(crate) fn clear_structured_failure(&mut self) {
        self.failed = false;
        self.failure_code = 0;
    }

    pub(crate) fn acquire_granule_read(
        &mut self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let current_version = self.current_version_for_granule(granule, current_version)?;
        let action = self.acquire_granule_read_authority(transaction, granule, current_version)?;
        let refresh_after_abort = action.aborted_transaction().is_some();
        self.handle_conflict_action(action)?;
        if refresh_after_abort {
            let current_version = self.current_version_for_granule(granule, current_version)?;
            self.refresh_read_version_authority(transaction, granule, current_version)?;
        }
        Ok(self.read_granules.insert(granule))
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let current_version = self.current_version_for_granule(granule, current_version)?;
        let action = self.acquire_granule_write_authority(transaction, granule, current_version)?;
        let refresh_after_abort = action.aborted_transaction().is_some();
        self.handle_conflict_action(action)?;
        if refresh_after_abort {
            let current_version = self.current_version_for_granule(granule, current_version)?;
            self.refresh_read_version_authority(transaction, granule, current_version)?;
        }
        self.read_granules.insert(granule);
        Ok(self.write_granules.insert(granule))
    }

    pub(crate) fn owns_granule_read(&self, granule: GranuleId) -> bool {
        self.read_granules.contains(&granule)
    }

    pub(crate) fn owns_granule_write(&self, granule: GranuleId) -> bool {
        self.write_granules.contains(&granule)
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
        let mut current_version_fn = current_version_fn;
        self.validate_active_read_granules_with(&mut current_version_fn)?;
        self.validate_active_writes_with(&mut current_version_fn)?;
        let records = self.staged_records()?;
        self.begin_terminal_commit()?;
        for record in records {
            apply(&record)?;
        }
        self.complete_commit()
    }

    pub(crate) fn validate_active_reads_with<F>(&self, current_version_fn: F) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        let mut current_version_fn = current_version_fn;
        self.validate_active_read_granules_with(&mut current_version_fn)
    }

    pub(crate) fn active_read_granules(&self) -> Result<Vec<GranuleId>> {
        let transaction = self.active_transaction_required()?;
        if self.shared_region_runtime.is_some() {
            return Ok(self.read_granules.iter().copied().collect());
        }
        Ok(self.concurrency.read_granules_for_transaction(transaction))
    }

    pub(crate) fn active_write_granules(&self) -> Result<Vec<GranuleId>> {
        self.active_transaction_required()?;
        Ok(self.write_granules.iter().copied().collect())
    }

    pub(crate) fn validate_active_read(
        &self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        let current_version = self.current_version_for_granule(granule, current_version)?;
        self.validate_granule_read_authority(transaction, granule, current_version)
    }

    pub(crate) fn versioned_granule_version(&self, granule: GranuleId) -> u64 {
        self.granule_versions.get(&granule).copied().unwrap_or(0)
    }

    pub(super) fn current_version_for_granule(
        &self,
        granule: GranuleId,
        backend_version: u64,
    ) -> Result<u64> {
        if granule_uses_transaction_state_version(granule) {
            if let Some(runtime) = &self.shared_region_runtime {
                runtime.versioned_granule_version(granule)
            } else {
                Ok(self.versioned_granule_version(granule))
            }
        } else {
            Ok(backend_version)
        }
    }

    fn acquire_granule_read_authority(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.acquire_granule_read(transaction, granule, current_version)
        } else {
            self.concurrency
                .acquire_granule_read(transaction, granule, current_version)
        }
    }

    fn acquire_granule_write_authority(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<TransactionConflictAction> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.acquire_granule_write(transaction, granule, current_version)
        } else {
            self.concurrency
                .acquire_granule_write(transaction, granule, current_version)
        }
    }

    fn refresh_read_version_authority(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.refresh_read_version(transaction, granule, current_version)
        } else {
            self.concurrency
                .refresh_read_version(transaction, granule, current_version);
            Ok(())
        }
    }

    fn validate_granule_read_authority(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.validate_granule_read(transaction, granule, current_version)
        } else {
            self.concurrency
                .validate_read(transaction, granule, current_version)
        }
    }

    fn validate_granule_write_authority(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.validate_granule_write(transaction, granule, current_version)
        } else {
            self.concurrency
                .validate_write(transaction, granule, current_version)
        }
    }

    fn release_transaction_authority(&mut self, transaction: TransactionId) -> Result<()> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.release_transaction(transaction)
        } else {
            self.concurrency.release_transaction_result(transaction)
        }
    }

    pub(crate) fn commit_transaction_authority(
        &mut self,
        transaction: TransactionId,
    ) -> Result<()> {
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.commit_transaction_result(transaction)
        } else {
            self.concurrency.commit_transaction_result(transaction)
        }
    }

    fn validate_active_read_granules_with<F>(&self, current_version_fn: &mut F) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        for granule in self.active_read_granules()? {
            let current_version = current_version_fn(granule)?;
            self.validate_active_read(granule, current_version)?;
        }
        Ok(())
    }

    fn validate_active_writes_with<F>(&self, current_version_fn: &mut F) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        let transaction = self.active_transaction_required()?;
        for granule in self.active_write_granules()? {
            let current_version = current_version_fn(granule)?;
            self.validate_granule_write_authority(transaction, granule, current_version)?;
        }
        Ok(())
    }

    fn take_shared_conflict_aborted_transaction(&self, transaction: TransactionId) -> Result<bool> {
        let Some(runtime) = &self.shared_region_runtime else {
            return Ok(false);
        };
        runtime.take_conflict_aborted_transaction(transaction)
    }

    pub(crate) fn begin_terminal_commit(&mut self) -> Result<()> {
        self.ensure_active()?;
        if self.terminal_commit_active {
            return Ok(());
        }
        match self.try_begin_terminal_commit() {
            Ok(()) => Ok(()),
            Err(error) if Self::is_conflict_aborted_error(&error) => {
                self.active_conflict_aborted = true;
                self.fail_active_conflict_aborted_without_object_cleanup()
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn begin_terminal_commit_with_object_cleanup(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        self.ensure_active()?;
        if self.terminal_commit_active {
            return Ok(());
        }
        match self.try_begin_terminal_commit() {
            Ok(()) => Ok(()),
            Err(error) if Self::is_conflict_aborted_error(&error) => {
                self.active_conflict_aborted = true;
                self.fail_active_conflict_aborted_with_object_cleanup(object_table)
            }
            Err(error) => Err(error),
        }
    }

    fn try_begin_terminal_commit(&mut self) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        if self.active_conflict_aborted {
            bail!("transaction was conflict-aborted by another transaction");
        }
        let Some(runtime) = &self.shared_region_runtime else {
            self.terminal_commit_active = true;
            return Ok(());
        };
        runtime.begin_terminal_commit(transaction)?;
        self.terminal_commit_active = true;
        Ok(())
    }

    fn fail_active_conflict_aborted_without_object_cleanup(&mut self) -> Result<()> {
        if self.allocated_objects.is_empty()
            && self.pending_conflict_aborted_allocated_objects.is_empty()
        {
            self.finish_active_conflict_aborted_without_object_cleanup()?;
            bail!("transaction was conflict-aborted by another transaction");
        }
        bail!(
            "transaction was conflict-aborted by another transaction and requires object-aware cleanup for allocated objects"
        )
    }

    fn fail_active_conflict_aborted_with_object_cleanup(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        self.abort_allocated_objects(object_table)?;
        bail!("transaction was conflict-aborted by another transaction");
    }

    fn finish_active_conflict_aborted_without_object_cleanup(&mut self) -> Result<()> {
        self.retry_post_commit_linear_undo_retirement();
        self.bump_active_versioned_write_granules()?;
        self.clear_active()?;
        Ok(())
    }

    fn is_conflict_aborted_error(error: &impl core::fmt::Display) -> bool {
        error
            .to_string()
            .contains("transaction was conflict-aborted by another transaction")
    }

    fn handle_conflict_action(&mut self, action: TransactionConflictAction) -> Result<()> {
        if let Some(owner) = action.would_wait_owner() {
            bail!(
                "transaction conflict would wait for transaction {}",
                owner.as_raw()
            );
        }
        self.discard_conflict_aborted_transaction(action.aborted_transaction())
    }

    pub(super) fn discard_conflict_aborted_transaction(
        &mut self,
        transaction: Option<TransactionId>,
    ) -> Result<()> {
        let Some(transaction) = transaction else {
            return Ok(());
        };
        if let Some(workspace) = self.suspended.remove(&transaction) {
            let _ = self.take_shared_conflict_aborted_transaction(transaction)?;
            self.pending_conflict_aborted_allocated_objects
                .extend(workspace.allocated_objects.iter().rev().copied());
            self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
        }
        Ok(())
    }

    pub(crate) fn drain_conflict_aborted_allocated_objects(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<bool> {
        let pending = mem::take(&mut self.pending_conflict_aborted_allocated_objects);
        let mut freed = false;
        for (index, object_id) in pending.iter().copied().enumerate() {
            if let Err(error) = object_table.free(object_id) {
                self.pending_conflict_aborted_allocated_objects
                    .extend(pending[index..].iter().copied());
                return Err(error);
            }
            freed = true;
        }
        Ok(freed)
    }

    pub(super) fn ensure_no_pending_conflict_aborted_allocated_objects(&self) -> Result<()> {
        ensure!(
            self.pending_conflict_aborted_allocated_objects.is_empty(),
            "generic transaction terminal path requires object-aware cleanup for conflict-aborted allocated objects"
        );
        Ok(())
    }

    pub(super) fn ensure_no_active_allocated_objects_for_generic_abort(&self) -> Result<()> {
        ensure!(
            self.allocated_objects.is_empty(),
            "generic transaction abort requires object-aware cleanup for allocated objects"
        );
        Ok(())
    }

    pub(super) fn bump_active_versioned_write_granules(&mut self) -> Result<()> {
        let granules = self.write_granules.iter().copied().collect::<Vec<_>>();
        self.bump_versioned_granules(granules)
    }

    pub(super) fn bump_versioned_granules<I>(&mut self, granules: I) -> Result<()>
    where
        I: IntoIterator<Item = GranuleId>,
    {
        if let Some(runtime) = &self.shared_region_runtime {
            return runtime.bump_versioned_granules(granules);
        }
        for granule in granules {
            if !granule_uses_transaction_state_version(granule) {
                continue;
            }
            let version = self.granule_versions.entry(granule).or_insert(0);
            *version = version
                .checked_add(1)
                .context("transaction granule version overflow")?;
        }
        Ok(())
    }

    pub(crate) fn complete_commit(&mut self) -> Result<()> {
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        self.bump_active_versioned_write_granules()?;
        self.clear_active()?;
        Ok(())
    }

    pub(crate) fn complete_commit_with_persistent_root_delta(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        if self.shared_region_runtime.is_some() {
            self.commit_shared_persistent_root_delta_before_complete_commit(delta)?;
            self.bump_active_versioned_write_granules()?;
            self.clear_active()?;
            return Ok(());
        }
        self.bump_active_versioned_write_granules()?;
        self.clear_active()?;
        self.apply_committed_persistent_root_delta(delta)?;
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
        for (key, &new_elements) in &self.staged_table_sizes {
            let GranuleId::TTableSize {
                instance,
                table_index,
            } = *key
            else {
                bail!("staged table size map contains non-table-size key");
            };
            records.push(StagedRecord::TableSize {
                owner_instance: granule_owner_instance(instance),
                table_index,
                new_elements,
            });
        }
        for (key, &value) in &self.staged_table_elements {
            records.push(StagedRecord::TableElement {
                owner_instance: granule_owner_instance(key.instance),
                table_index: key.table_index,
                element_index: key.element_index,
                value,
            });
        }
        Ok(records)
    }

    pub(crate) fn abort(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.active_conflict_aborted = false;
        let _ =
            self.take_shared_conflict_aborted_transaction(self.active_transaction_required()?)?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.ensure_no_active_allocated_objects_for_generic_abort()?;
        self.retry_post_commit_linear_undo_retirement();
        self.bump_active_versioned_write_granules()?;
        self.clear_active()?;
        Ok(())
    }

    pub(crate) fn record_allocated_object(&mut self, object_id: ObjectId) -> Result<bool> {
        self.ensure_active()?;
        if self.allocated_objects.contains(&object_id) {
            return Ok(false);
        }
        self.allocated_objects.push(object_id);
        Ok(true)
    }

    pub(crate) fn abort_allocated_objects(&mut self, object_table: &mut ObjectTable) -> Result<()> {
        self.ensure_active()?;
        self.active_conflict_aborted = false;
        let _ =
            self.take_shared_conflict_aborted_transaction(self.active_transaction_required()?)?;
        self.retry_post_commit_linear_undo_retirement();
        let mut result = self
            .drain_conflict_aborted_allocated_objects(object_table)
            .map(|_| ());
        let allocated = self
            .allocated_objects
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();
        for (index, object_id) in allocated.iter().copied().enumerate() {
            if let Err(error) = object_table.free(object_id) {
                self.pending_conflict_aborted_allocated_objects
                    .extend(allocated[index..].iter().copied());
                if result.is_ok() {
                    result = Err(error);
                }
                break;
            }
        }
        if let Err(error) = self.bump_active_versioned_write_granules() {
            if result.is_ok() {
                result = Err(error);
            }
        }
        if let Err(error) = self.clear_active() {
            if result.is_ok() {
                result = Err(error);
            }
        }
        result
    }

    pub(crate) fn fail_allocated_objects(&mut self, object_table: &mut ObjectTable) -> Result<()> {
        self.fail_allocated_objects_with_code(object_table, 0)
    }

    pub(crate) fn fail_allocated_objects_with_code(
        &mut self,
        object_table: &mut ObjectTable,
        code: u32,
    ) -> Result<()> {
        self.abort_allocated_objects(object_table)?;
        self.failed = true;
        self.failure_code = code;
        Ok(())
    }

    pub(crate) fn fail(&mut self) -> Result<()> {
        self.abort()
    }

    pub(crate) fn fail_structured(&mut self) -> Result<()> {
        self.fail_structured_with_code(0)
    }

    pub(crate) fn fail_structured_with_code(&mut self, code: u32) -> Result<()> {
        self.abort()?;
        self.failed = true;
        self.failure_code = code;
        Ok(())
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
        self.acquire_granule_write(memory_size_granule_id(owner_instance, memory_index), 0)?;
        Ok(self
            .staged_memory_sizes
            .insert(
                memory_size_granule_id(owner_instance, memory_index),
                new_pages,
            )
            .is_none())
    }

    pub(crate) fn staged_memory_size_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Option<u64> {
        self.staged_memory_sizes
            .get(&memory_size_granule_id(owner_instance, memory_index))
            .copied()
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
        let key = global_granule_id(owner_instance, global_index);
        self.acquire_granule_write(key, 0)?;
        Ok(self.staged_globals.insert(key, value).is_none())
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

    pub(crate) fn stage_table_element_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        value: TableElementSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.acquire_table_granule_write_owned(owner_instance, table_index, element_index, 0)?;
        Ok(self
            .staged_table_elements
            .insert(
                table_element_key(owner_instance, table_index, element_index),
                value,
            )
            .is_none())
    }

    pub(crate) fn staged_table_element_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
    ) -> Option<TableElementSnapshot> {
        self.staged_table_elements
            .get(&table_element_key(
                owner_instance,
                table_index,
                element_index,
            ))
            .copied()
    }

    pub(crate) fn stage_table_size_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        new_elements: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.acquire_table_size_write_owned(owner_instance, table_index, 0)?;
        Ok(self
            .staged_table_sizes
            .insert(
                table_size_granule_id(owner_instance, table_index),
                new_elements,
            )
            .is_none())
    }

    pub(crate) fn staged_table_size_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> Option<u64> {
        self.staged_table_sizes
            .get(&table_size_granule_id(owner_instance, table_index))
            .copied()
    }

    pub(crate) fn stage_memory_granule(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = memory_granule_id_from_u64(None, memory_index, granule_index);
        self.acquire_granule_write(key, 0)?;
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
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            self.acquire_granule_write(key, 0)?;
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

            self.acquire_granule_write(key, granule.version)?;
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
            let chunk_len = (range.end - current).min(granule_range.end - current);

            self.acquire_granule_read(key, 0)?;
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

            self.acquire_granule_read(key, granule.version)?;
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
            owner_instance_key: Some(instance),
            memory_index,
            addr,
            len,
        });
        Ok(self.scratch.as_mut_ptr())
    }

    pub(crate) fn set_tmemory_store_scratch(
        &mut self,
        instance: InstanceId,
        owner_instance_key: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: Vec<u8>,
    ) -> Result<*mut u8> {
        self.ensure_active()?;
        debug_assert!(self.pending_memory_store.is_none());
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            owner_instance_key,
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
            pending.owner_instance_key,
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
            pending.owner_instance_key,
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
        let region = self.shared_region_runtime_for_publication().cloned();
        if let Some(region) = region {
            return region.with_shared_file_backed_tmemory_commit_read_lock(|| {
                self.commit_tmemory_owned_inner(owner_instance, memory_index, tmemory)
            });
        }
        self.commit_tmemory_owned_inner(owner_instance, memory_index, tmemory)
    }

    fn commit_tmemory_owned_inner(
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

        let transaction_id = self.active_transaction_required_raw()?;
        let stream_id = if let Some(region) = self.shared_region_runtime_for_publication() {
            region.current_thread_log_segment()?.stream_id()
        } else {
            u32::try_from(transaction_id)
                .context("transaction id does not fit durable tmemory stream id")?
        };
        let txid = u32::try_from(transaction_id)
            .context("transaction id does not fit durable transaction id")?;
        let staged = staged
            .iter()
            .map(|(_, granule_index, bytes)| {
                Ok((
                    u64::try_from(*granule_index)
                        .context("tmemory granule index does not fit durable log")?,
                    bytes.clone(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if tmemory.backend() == TMemoryBackend::VMemory {
            tmemory.commit_staged_tmemory_granules_direct(&staged)?;
        } else {
            let mut final_marker = None;
            for (granule_index, bytes) in &staged {
                let undo = tmemory.prepare_tmemory_undo_record(
                    owner_instance.map(InstanceId::as_u32),
                    memory_index,
                    *granule_index,
                    bytes,
                )?;
                final_marker =
                    Some(self.publish_tmemory_undo_before_in_place_write(stream_id, txid, &undo)?);
            }
            for (granule_index, bytes) in &staged {
                tmemory.commit_staged_tmemory_granule(*granule_index, bytes)?;
            }
            if let Some(marker) = final_marker {
                self.publish_commit_lp(stream_id, txid, marker)?;
            }
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
        self.acquire_granule_read(
            memory_granule_id_from_u64(owner_instance, memory_index, granule_index),
            0,
        )
    }

    pub(crate) fn acquire_memory_size_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_read(memory_size_granule_id(owner_instance, memory_index), 0)
    }

    pub(crate) fn acquire_memory_size_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_write(memory_size_granule_id(owner_instance, memory_index), 0)
    }

    pub(crate) fn acquire_global_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_read(global_granule_id(owner_instance, global_index), 0)
    }

    pub(crate) fn acquire_global_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_write(global_granule_id(owner_instance, global_index), 0)
    }

    pub(crate) fn acquire_object_read(
        &mut self,
        object_table: &mut ObjectTable,
        object_id: ObjectId,
    ) -> Result<bool> {
        let acquired = self.acquire_granule_read(
            object_table.granule_id(object_id)?,
            object_table.version(object_id)?,
        )?;
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        Ok(acquired)
    }

    pub(crate) fn acquire_object_write(
        &mut self,
        object_table: &mut ObjectTable,
        object_id: ObjectId,
    ) -> Result<bool> {
        let acquired = self.acquire_granule_write(
            object_table.granule_id(object_id)?,
            object_table.version(object_id)?,
        )?;
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        Ok(acquired)
    }

    pub(crate) fn acquire_tref_read_for_transaction_ref_handle(
        &mut self,
        object_table: &mut ObjectTable,
        handle: u32,
    ) -> Result<bool> {
        let Some(object_id) =
            object_table.known_persistent_object_id_for_transaction_ref_handle(handle)?
        else {
            return Ok(false);
        };
        self.acquire_object_read(object_table, object_id)
    }

    pub(crate) fn acquire_tref_write_for_transaction_ref_handle(
        &mut self,
        object_table: &mut ObjectTable,
        handle: u32,
    ) -> Result<bool> {
        let Some(object_id) =
            object_table.known_persistent_object_id_for_transaction_ref_handle(handle)?
        else {
            return Ok(false);
        };
        self.acquire_object_write(object_table, object_id)
    }

    pub(crate) fn read_object_payload(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<ObjectPayload> {
        if object_table.is_persistent(object_id)? {
            let granule = object_table.granule_id(object_id)?;
            ensure!(
                self.owns_granule_read(granule),
                "transactional object read permission was not acquired"
            );
        }
        if let Some(payload) = self.staged_objects.get(&object_id) {
            return Ok(payload.clone());
        }
        object_table.payload(object_id)
    }

    pub(crate) fn stage_object_payload(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        payload: ObjectPayload,
    ) -> Result<bool> {
        ensure!(
            object_table.kind(object_id)? == payload.kind(),
            "object payload kind does not match object table slot kind"
        );
        if object_table.is_persistent(object_id)? {
            let granule = object_table.granule_id(object_id)?;
            ensure!(
                self.owns_granule_write(granule),
                "transactional object write permission was not acquired"
            );
        }
        Ok(self.staged_objects.insert(object_id, payload).is_none())
    }

    pub(crate) fn read_struct_field(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        field_index: usize,
    ) -> Result<ObjectValue> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Struct(fields) = payload else {
            bail!("object is not a struct");
        };
        fields
            .get(field_index)
            .cloned()
            .context("out of bounds struct access")
    }

    pub(crate) fn stage_struct_field(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        field_index: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Struct(mut fields) = payload else {
            bail!("object is not a struct");
        };
        let field = fields
            .get_mut(field_index)
            .context("out of bounds struct access")?;
        *field = value;
        self.stage_object_payload(object_table, object_id, ObjectPayload::Struct(fields))?;
        Ok(())
    }

    pub(crate) fn read_array_len(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<usize> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(elements) = payload else {
            bail!("object is not an array");
        };
        Ok(elements.len())
    }

    pub(crate) fn read_array_element(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        element_index: usize,
    ) -> Result<ObjectValue> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(elements) = payload else {
            bail!("object is not an array");
        };
        elements
            .get(element_index)
            .cloned()
            .context("out of bounds array access")
    }

    pub(crate) fn stage_array_element(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        element_index: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let element = elements
            .get_mut(element_index)
            .context("out of bounds array access")?;
        *element = value;
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn fill_array_range(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        start: usize,
        len: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let end = checked_array_range_end(start, len)?;
        ensure!(end <= elements.len(), "out of bounds array access");
        elements[start..end].fill(value);
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn write_array_range(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        start: usize,
        values: Vec<ObjectValue>,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let end = checked_array_range_end(start, values.len())?;
        ensure!(end <= elements.len(), "out of bounds array access");
        elements[start..end].clone_from_slice(&values);
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn copy_array_range(
        &mut self,
        object_table: &ObjectTable,
        dst_object_id: ObjectId,
        dst_start: usize,
        src_object_id: ObjectId,
        src_start: usize,
        len: usize,
    ) -> Result<()> {
        let src_payload = self.read_object_payload(object_table, src_object_id)?;
        let ObjectPayload::Array(src_elements) = src_payload else {
            bail!("source object is not an array");
        };
        let src_end = checked_array_range_end(src_start, len)?;
        ensure!(src_end <= src_elements.len(), "out of bounds array access");
        let copied = src_elements[src_start..src_end].to_vec();

        let dst_payload = self.read_object_payload(object_table, dst_object_id)?;
        let ObjectPayload::Array(mut dst_elements) = dst_payload else {
            bail!("destination object is not an array");
        };
        let dst_end = checked_array_range_end(dst_start, len)?;
        ensure!(dst_end <= dst_elements.len(), "out of bounds array access");
        dst_elements[dst_start..dst_end].clone_from_slice(&copied);
        self.stage_object_payload(
            object_table,
            dst_object_id,
            ObjectPayload::Array(dst_elements),
        )?;
        Ok(())
    }

    pub(crate) fn create_i31(&self, value: i32) -> Result<I31Value> {
        self.ensure_active()?;
        Ok(I31Value::new(value))
    }

    pub(crate) fn commit_object_payloads(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<bool> {
        self.commit_object_payloads_with(object_table, |_| Ok(()))
    }

    pub(crate) fn commit_object_payloads_into(
        &mut self,
        object_table: &mut ObjectTable,
        pending_publications: &mut Vec<persist::PendingPublication>,
    ) -> Result<bool> {
        self.commit_object_payloads_with(object_table, |publication| {
            pending_publications.push(publication);
            Ok(())
        })
    }

    pub(crate) fn staged_persistent_root_delta(
        &self,
        object_table: &ObjectTable,
    ) -> Result<PersistentRootDelta> {
        self.ensure_active()?;
        let mut delta = PersistentRootDelta::default();

        for (&key, &value) in &self.staged_globals {
            let GranuleId::TGlobal {
                instance,
                global_index,
            } = key
            else {
                bail!("staged global map contains non-global key");
            };
            let root_key = PersistentRootKey::Global {
                instance,
                global_index,
            };
            let roots = delta.roots.entry(root_key).or_default();
            let root = match value {
                GlobalSnapshot::GcRef(gc_ref) => self
                    .persistent_object_id_for_live_bridge_after_completed_promotion(
                        object_table,
                        gc_ref,
                    )?,
                _ => None,
            };
            if let Some(root) = root {
                roots.insert(root);
            }
        }

        for (&key, &value) in &self.staged_table_elements {
            let root_key = PersistentRootKey::TableElement(key);
            let roots = delta.roots.entry(root_key).or_default();
            let root = match value {
                TableElementSnapshot::GcRef(gc_ref) => self
                    .persistent_object_id_for_live_bridge_after_completed_promotion(
                        object_table,
                        gc_ref,
                    )?,
                TableElementSnapshot::FuncRef(_) => None,
            };
            if let Some(root) = root {
                roots.insert(root);
            }
        }

        Ok(delta)
    }

    pub(crate) fn persistent_root_publications(
        &mut self,
        delta: &PersistentRootDelta,
    ) -> Result<Vec<persist::PendingPublication>> {
        self.ensure_active()?;
        if let Some(runtime) = &self.shared_region_runtime {
            let newly_reserved = runtime.reserve_persistent_root_versions(
                delta
                    .roots
                    .keys()
                    .filter(|key| !self.pending_persistent_root_versions.contains_key(key))
                    .copied(),
            )?;
            for (key, version) in newly_reserved {
                self.pending_persistent_root_versions.insert(key, version);
            }
        }
        let mut publications = Vec::new();
        for (&key, roots) in &delta.roots {
            let version = if self.shared_region_runtime.is_some() {
                self.pending_persistent_root_versions
                    .get(&key)
                    .copied()
                    .context("shared persistent root publication version was not reserved")?
            } else {
                self.persistent_root_versions
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("persistent root publication version overflow")?
            };
            match key {
                PersistentRootKey::Global {
                    instance,
                    global_index,
                } => {
                    let roots = roots
                        .iter()
                        .map(|root| Some(root.object_index))
                        .chain(roots.is_empty().then_some(None));
                    publications.push(persist::PendingPublication::persistent_global_root(
                        pack_persistent_global_root_index(instance, global_index)?,
                        version,
                        roots,
                    )?);
                }
                PersistentRootKey::TableElement(key) => {
                    publications.push(persist::PendingPublication::persistent_table_root(
                        pack_persistent_table_root_index(key)?,
                        version,
                        roots.iter().map(|root| Some(root.object_index)),
                    )?);
                }
                PersistentRootKey::Recovered => {}
            }
        }
        Ok(publications)
    }

    pub(crate) fn tmemory_size_publications(&self) -> Result<Vec<persist::PendingPublication>> {
        self.ensure_active()?;
        let mut publications = Vec::new();
        for (&granule, &new_pages) in &self.staged_memory_sizes {
            let GranuleId::TMemorySize {
                instance,
                memory_index,
            } = granule
            else {
                bail!("staged memory size map contains non-size key");
            };
            let version = self
                .current_version_for_granule(granule, 0)?
                .checked_add(1)
                .context("tmemory size publication version overflow")?;
            publications.push(persist::PendingPublication::tmemory_size(
                granule_owner_instance(instance).map(InstanceId::as_u32),
                memory_index,
                u32::try_from(version)
                    .context("tmemory size publication version does not fit durable log")?,
                new_pages,
            )?);
        }
        Ok(publications)
    }

    pub(crate) fn persistent_gc_commit_delta(
        &self,
        object_table: &ObjectTable,
        publications: &[persist::PendingPublication],
    ) -> Result<object_gc::PersistentGcCommitDelta> {
        self.ensure_active()?;
        let mut delta = object_gc::PersistentGcCommitDelta::default();
        delta.invalidates_reachable_cache = !self.staged_globals.is_empty()
            || !self.staged_table_elements.is_empty()
            || !publications.is_empty();
        for &value in self.staged_globals.values() {
            let root = match value {
                GlobalSnapshot::GcRef(gc_ref) => self
                    .persistent_object_id_for_live_bridge_after_completed_promotion(
                        object_table,
                        gc_ref,
                    )?,
                _ => None,
            };
            if let Some(root) = root {
                delta.new_roots.insert(root);
            }
        }
        for &value in self.staged_table_elements.values() {
            let root = match value {
                TableElementSnapshot::GcRef(gc_ref) => self
                    .persistent_object_id_for_live_bridge_after_completed_promotion(
                        object_table,
                        gc_ref,
                    )?,
                TableElementSnapshot::FuncRef(_) => None,
            };
            if let Some(root) = root {
                delta.new_roots.insert(root);
            }
        }
        for publication in publications {
            let (_, object_index) =
                crate::runtime::vm::unpack_object_granule_id(publication.logical_id)?;
            let from = ObjectId { object_index };
            for to in object_table.trace_object_ids(from)? {
                if let Ok(slot) = object_table.live_slot(to)
                    && slot.persistent
                {
                    delta
                        .edges
                        .insert(object_gc::PersistentObjectEdge { from, to });
                }
            }
        }
        Ok(delta)
    }

    pub(crate) fn observe_persistent_gc_commit_delta_after_commit(
        &mut self,
        object_table: &ObjectTable,
        delta: &PersistentGcCommitDelta,
    ) -> Result<PersistentGcStepReport> {
        ensure!(
            self.active.is_none() && current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active"
        );
        if delta.invalidates_reachable_cache {
            self.persistent_gc_state = None;
        }
        let committed_roots = if self.persistent_gc_state.is_none() {
            Some(self.persistent_root_ids()?)
        } else {
            None
        };
        if self.persistent_gc_state.is_none()
            && delta.new_roots.is_empty()
            && delta.edges.is_empty()
            && committed_roots.as_ref().is_none_or(BTreeSet::is_empty)
        {
            return Ok(PersistentGcStepReport::default());
        }
        let state = match self.persistent_gc_state.as_mut() {
            Some(state) => state,
            None => self.persistent_gc_state.insert(PersistentGcState::new(
                object_table,
                committed_roots.unwrap_or_default(),
            )?),
        };
        state.observe_commit_delta(object_table, delta, PersistentGcBudget::objects(1))
    }

    pub(crate) fn observe_persistent_gc_commit_delta_after_commit_best_effort(
        &mut self,
        object_table: &ObjectTable,
        delta: &PersistentGcCommitDelta,
    ) -> PersistentGcStepReport {
        match self.observe_persistent_gc_commit_delta_after_commit(object_table, delta) {
            Ok(report) => report,
            Err(_) => {
                self.persistent_gc_state = None;
                PersistentGcStepReport::default()
            }
        }
    }

    pub(crate) fn apply_committed_persistent_root_delta(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        let applying_during_shared_terminal_commit = self.shared_region_runtime.is_some()
            && self.active.is_some()
            && current_thread_transaction() == self.active
            && self.terminal_commit_active;
        ensure!(
            applying_during_shared_terminal_commit
                || (self.active.is_none() && current_thread_transaction().is_none()),
            "committed persistent root delta can only be applied after complete_commit"
        );
        if delta.is_empty() {
            return Ok(());
        }

        if let Some(runtime) = &self.shared_region_runtime {
            let root_keys = delta.roots.keys().copied().collect::<Vec<_>>();
            let reserved_versions = delta
                .roots
                .keys()
                .copied()
                .map(|key| {
                    let version = self
                        .pending_persistent_root_versions
                        .get(&key)
                        .copied()
                        .context("shared persistent root publication version was not reserved")?;
                    Ok((key, version))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            runtime.apply_committed_persistent_root_delta(delta, reserved_versions)?;
            for key in root_keys {
                self.pending_persistent_root_versions.remove(&key);
            }
            return Ok(());
        }

        let next_versions = delta
            .roots
            .keys()
            .copied()
            .map(|key| {
                let next_version = self
                    .persistent_root_versions
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("persistent root version overflow")?;
                Ok((key, next_version))
            })
            .collect::<Result<Vec<_>>>()?;

        for (key, roots) in delta.roots {
            if roots.is_empty() {
                self.persistent_roots.remove(&key);
            } else {
                self.persistent_roots.insert(key, roots);
            }
        }
        for (key, version) in next_versions {
            self.persistent_root_versions.insert(key, version);
        }
        Ok(())
    }

    pub(crate) fn commit_shared_persistent_root_delta_before_complete_commit(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        self.ensure_active()?;
        ensure!(
            self.shared_region_runtime.is_some(),
            "shared persistent root commit requires a shared region runtime"
        );
        ensure!(
            self.terminal_commit_active,
            "shared persistent root commit requires an active terminal commit"
        );
        self.apply_committed_persistent_root_delta(delta)
    }

    pub(crate) fn finish_committed_cleanup_after_durable_commit_error(&mut self) -> Result<()> {
        self.ensure_active()?;
        ensure!(
            self.terminal_commit_active,
            "durable commit cleanup requires an active terminal commit"
        );
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        let mut result = self.bump_active_versioned_write_granules();
        if let Err(error) = self.clear_active()
            && result.is_ok()
        {
            result = Err(error);
        }
        result
    }

    pub(crate) fn persistent_root_ids(&self) -> Result<BTreeSet<ObjectId>> {
        if let Some(runtime) = &self.shared_region_runtime {
            return runtime.persistent_root_ids();
        }
        Ok(self
            .persistent_roots
            .values()
            .flat_map(|roots| roots.iter().copied())
            .collect())
    }

    pub(crate) fn persistent_mark_sweep_collect(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport> {
        ensure!(
            self.active.is_none()
                && self.suspended.is_empty()
                && current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active or suspended"
        );
        let _shared_gc_region_permit = if let Some(runtime) = &self.shared_region_runtime {
            Some(runtime.begin_persistent_gc()?)
        } else {
            None
        };
        let roots = self.persistent_root_ids()?;
        let report = objects.persistent_mark_sweep_from_roots(roots)?;
        self.persistent_gc_state = None;
        Ok(report)
    }

    pub(crate) fn persistent_gc_maintenance_step(
        &mut self,
        object_table: &ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        ensure!(
            self.active.is_none()
                && self.suspended.is_empty()
                && current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active or suspended"
        );
        let _shared_gc_region_permit = if let Some(runtime) = &self.shared_region_runtime {
            Some(runtime.begin_persistent_gc()?)
        } else {
            None
        };
        let roots = if self.persistent_gc_state.is_none() {
            Some(self.persistent_root_ids()?)
        } else {
            None
        };
        let state = match self.persistent_gc_state.as_mut() {
            Some(state) => state,
            None => self.persistent_gc_state.insert(PersistentGcState::new(
                object_table,
                roots.unwrap_or_default(),
            )?),
        };
        state.mark_step(object_table, budget)
    }

    pub(crate) fn finish_persistent_gc_cycle_and_sweep(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>> {
        ensure!(
            self.active.is_none()
                && self.suspended.is_empty()
                && current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active or suspended"
        );
        let _shared_gc_region_permit = if let Some(runtime) = &self.shared_region_runtime {
            Some(runtime.begin_persistent_gc()?)
        } else {
            None
        };
        let Some(mut state) = self.persistent_gc_state.take() else {
            return Ok(None);
        };
        while !state.is_complete() {
            let pending = state.pending_object_count();
            state.mark_step(objects, PersistentGcBudget::objects(pending.max(1)))?;
        }
        let mark = state.into_report(objects)?;
        mark.ensure_sweepable()?;
        let sweep = objects.apply_volatile_persistent_sweep(&mark)?;
        Ok(Some(PersistentMarkSweepReport { mark, sweep }))
    }

    pub(crate) fn compact_persistent_object_chunks(
        &mut self,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport> {
        ensure!(
            self.active.is_none()
                && self.suspended.is_empty()
                && current_thread_transaction().is_none(),
            "persistent object compaction cannot run while a transaction is active or suspended"
        );
        let _shared_gc_region_permit = if let Some(runtime) = &self.shared_region_runtime {
            Some(runtime.begin_persistent_gc()?)
        } else {
            None
        };
        recovery_report.mark.ensure_sweepable()?;

        #[derive(Default)]
        struct ChunkCandidate {
            reachable_objects: BTreeSet<ObjectId>,
            old_locations: Vec<PersistentRecoveredRecordLocation>,
            has_unreachable: bool,
        }

        let recovered = self
            .durable_log
            .recover_region_snapshot()?
            .context("persistent object compaction requires a durable block region backend")?;
        let winners = recovered.committed_object_winners()?;
        let mut chunks = BTreeMap::<u32, ChunkCandidate>::new();
        for winner in &winners {
            let object_id = ObjectId {
                object_index: winner.object_id,
            };
            let Some(chunk_start) = self
                .durable_log
                .object_data_chunk_start_for_block(winner.data_block)?
            else {
                continue;
            };
            let location = recovered_record_location_from_winner(winner);
            let chunk = chunks.entry(chunk_start).or_default();
            if recovery_report.mark.reachable.contains(&object_id) {
                chunk.reachable_objects.insert(object_id);
                chunk.old_locations.push(location);
            } else if recovery_report
                .mark
                .unreachable_persistent
                .contains(&object_id)
            {
                chunk.has_unreachable = true;
                chunk.old_locations.push(location);
            }
        }

        let mut selected_chunks = Vec::new();
        let mut copied_objects = BTreeSet::new();
        let mut old_locations = Vec::new();
        let mut skipped_chunks = Vec::new();
        for (chunk_start, chunk) in chunks {
            if chunk.reachable_objects.is_empty() || !chunk.has_unreachable {
                skipped_chunks.push(chunk_start);
                continue;
            }
            selected_chunks.push(chunk_start);
            copied_objects.extend(chunk.reachable_objects);
            old_locations.extend(chunk.old_locations);
        }

        if copied_objects.is_empty() {
            return Ok(PersistentObjectCompactionReport {
                skipped_chunks,
                ..PersistentObjectCompactionReport::default()
            });
        }

        let mut publications = Vec::new();
        for object_id in &copied_objects {
            publications.push(objects.persistent_gc_copy_publication(*object_id)?);
        }

        let stream_id =
            u32::try_from(self.next_id).context("GC maintenance transaction id overflow")?;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("transaction id overflow")?;
        let marker = self
            .publish_object_publications_before_commit(
                stream_id,
                stream_id,
                objects,
                &publications,
            )?
            .context("GC maintenance transaction did not publish copied object records")?;
        self.publish_commit_lp(stream_id, stream_id, marker)?;
        for publication in &publications {
            objects.install_persistent_gc_copied_publication(publication)?;
        }

        let recovered_after = self
            .durable_log
            .recover_region_snapshot()?
            .context("persistent object compaction requires a durable block region backend")?;
        let winners_after = recovered_after.committed_object_winners()?;
        let reachable_after = winners_after
            .iter()
            .filter_map(|winner| {
                let object_id = ObjectId {
                    object_index: winner.object_id,
                };
                recovery_report
                    .mark
                    .reachable
                    .contains(&object_id)
                    .then(|| recovered_record_location_from_winner(winner))
            })
            .collect::<Vec<_>>();
        let retired_chunks = self
            .durable_log
            .retire_whole_dead_object_chunks(&reachable_after, &old_locations)?;
        let retired_set = retired_chunks.iter().copied().collect::<BTreeSet<_>>();
        skipped_chunks.extend(
            selected_chunks
                .into_iter()
                .filter(|chunk| !retired_set.contains(chunk)),
        );

        Ok(PersistentObjectCompactionReport {
            copied_objects: copied_objects.into_iter().collect(),
            retired_chunks,
            skipped_chunks,
        })
    }

    pub(crate) fn install_recovered_persistent_roots<I>(&mut self, roots: I) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        ensure!(
            self.active.is_none() && current_thread_transaction().is_none(),
            "recovered persistent roots can only be installed outside an active transaction"
        );
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.install_recovered_persistent_roots(roots)?;
            self.persistent_gc_state = None;
            return Ok(());
        }
        let roots = roots.into_iter().collect::<BTreeSet<_>>();
        if roots.is_empty() {
            self.persistent_roots.remove(&PersistentRootKey::Recovered);
        } else {
            self.persistent_roots
                .insert(PersistentRootKey::Recovered, roots);
        }
        self.persistent_gc_state = None;
        Ok(())
    }

    pub(crate) fn install_recovered_persistent_root_state<I, J>(
        &mut self,
        roots: I,
        versions: J,
    ) -> Result<()>
    where
        I: IntoIterator<Item = ObjectId>,
        J: IntoIterator<Item = (u64, u32)>,
    {
        ensure!(
            self.active.is_none() && current_thread_transaction().is_none(),
            "recovered persistent roots can only be installed outside an active transaction"
        );
        if let Some(runtime) = &self.shared_region_runtime {
            runtime.install_recovered_persistent_root_state(roots, versions)?;
            self.persistent_gc_state = None;
            return Ok(());
        }
        self.install_recovered_persistent_roots(roots)?;
        for (logical_id, version) in versions {
            let Some(key) = persistent_root_key_from_logical_id(logical_id)? else {
                continue;
            };
            self.persistent_root_versions
                .entry(key)
                .and_modify(|current| *current = (*current).max(version))
                .or_insert(version);
        }
        Ok(())
    }

    pub(super) fn commit_object_payloads_with<F>(
        &mut self,
        object_table: &mut ObjectTable,
        mut publish: F,
    ) -> Result<bool>
    where
        F: FnMut(persist::PendingPublication) -> Result<()>,
    {
        self.ensure_active()?;
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        self.validate_active_object_reads(object_table)?;
        if self.staged_objects.is_empty() {
            return Ok(false);
        }
        let updates = self
            .staged_objects
            .iter()
            .map(|(&object_id, payload)| {
                ensure!(
                    object_table.kind(object_id)? == payload.kind(),
                    "object payload kind does not match object table slot kind"
                );
                Ok((object_id, payload.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        for (object_id, payload) in updates {
            let persistent = object_table.is_persistent(object_id)?;
            if persistent {
                object_table.validate_persistent_payload_refs(&payload)?;
            }
            object_table.update_payload(object_id, payload)?;
            if persistent {
                publish(object_table.object_pending_publication(object_id)?)?;
            }
        }
        self.staged_objects.clear();
        Ok(true)
    }

    pub(crate) fn validate_active_object_reads(&self, object_table: &ObjectTable) -> Result<()> {
        for granule in self.active_read_granules()? {
            let Some(object_id) = object_granule_object_id(granule) else {
                continue;
            };
            self.validate_active_read(granule, object_table.version(object_id)?)?;
        }
        Ok(())
    }

    pub(crate) fn owns_memory_size_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> bool {
        self.owns_granule_read(memory_size_granule_id(owner_instance, memory_index))
    }

    pub(crate) fn owns_memory_size_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> bool {
        self.owns_granule_write(memory_size_granule_id(owner_instance, memory_index))
    }

    pub(crate) fn owns_global_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> bool {
        self.owns_granule_read(global_granule_id(owner_instance, global_index))
    }

    pub(crate) fn owns_global_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> bool {
        self.owns_granule_write(global_granule_id(owner_instance, global_index))
    }

    pub(crate) fn owns_object_read(&self, object_id: ObjectId) -> bool {
        self.owns_granule_read(GranuleId::Object { object_id })
    }

    pub(crate) fn owns_object_write(&self, object_id: ObjectId) -> bool {
        self.owns_granule_write(GranuleId::Object { object_id })
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
        let key = memory_granule_id_from_u64(owner_instance, memory_index, granule_index);
        self.acquire_granule_write(key, 0)?;
        Ok(self.staged_granules.insert(key, bytes).is_none())
    }

    pub(super) fn acquire_memory_write_ownership_range_owned(
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
            self.acquire_granule_write(key, 0)?;
            current = end.min(granule_end);
        }

        Ok(())
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
        self.owns_granule_read(memory_granule_id_from_u64(
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
        self.owns_granule_write(memory_granule_id_from_u64(
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
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.acquire_granule_read(key, current_version)
    }

    pub(crate) fn acquire_table_granule_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.acquire_granule_write(key, current_version)
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
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable read range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            inserted |= self.acquire_granule_read(key, current_version)?;
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
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable write range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            inserted |= self.acquire_granule_write(key, current_version)?;
        }
        Ok(inserted)
    }

    pub(crate) fn acquire_table_size_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_size_granule_id(owner_instance, table_index);
        self.acquire_granule_read(key, current_version)
    }

    pub(crate) fn acquire_table_size_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_size_granule_id(owner_instance, table_index);
        self.acquire_granule_write(key, current_version)
    }

    pub(crate) fn owns_table_granule_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        granule_index: u64,
    ) -> bool {
        self.owns_granule_read(table_granule_id_from_u64(
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
        self.owns_granule_write(table_granule_id_from_u64(
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
        self.owns_granule_read(table_size_granule_id(owner_instance, table_index))
    }

    pub(crate) fn owns_table_size_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> bool {
        self.owns_granule_write(table_size_granule_id(owner_instance, table_index))
    }

    pub(super) fn merge_staged_tmemory_range(
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

    pub(super) fn active_transaction_required(&self) -> Result<TransactionId> {
        ensure!(
            current_thread_transaction() == self.active,
            "thread transaction state does not match active transaction"
        );
        self.active_transaction()
            .context("transaction operation requires an active transaction")
    }

    pub(super) fn ensure_active(&self) -> Result<()> {
        ensure!(
            self.active.is_some(),
            "transaction operation requires an active transaction"
        );
        ensure!(
            current_thread_transaction() == self.active,
            "thread transaction state does not match active transaction"
        );
        Ok(())
    }

    pub(super) fn suspend_active_workspace(&mut self) -> Result<Option<TransactionId>> {
        let Some(transaction) = self.active.take() else {
            return Ok(None);
        };
        let workspace = self.take_workspace();
        ensure!(
            self.suspended.insert(transaction, workspace).is_none(),
            "transaction workspace is already suspended"
        );
        Ok(Some(transaction))
    }

    pub(super) fn take_workspace(&mut self) -> TransactionWorkspace {
        TransactionWorkspace {
            staged_globals: mem::take(&mut self.staged_globals),
            staged_granules: mem::take(&mut self.staged_granules),
            staged_memory_sizes: mem::take(&mut self.staged_memory_sizes),
            staged_table_sizes: mem::take(&mut self.staged_table_sizes),
            staged_table_elements: mem::take(&mut self.staged_table_elements),
            staged_objects: mem::take(&mut self.staged_objects),
            pending_persistent_root_versions: mem::take(&mut self.pending_persistent_root_versions),
            promoted_objects: mem::take(&mut self.promoted_objects),
            promoted_gc_refs: mem::take(&mut self.promoted_gc_refs),
            durable_leaf_gc_refs: mem::take(&mut self.durable_leaf_gc_refs),
            allocated_objects: mem::take(&mut self.allocated_objects),
            conflict_aborted: mem::take(&mut self.active_conflict_aborted),
            read_granules: mem::take(&mut self.read_granules),
            write_granules: mem::take(&mut self.write_granules),
            uncommitted_publication_streams: mem::take(&mut self.uncommitted_publication_streams),
            pending_linear_undo_chunks: mem::take(&mut self.pending_linear_undo_chunks),
            scratch: mem::take(&mut self.scratch),
            pending_memory_store: self.pending_memory_store.take(),
        }
    }

    pub(super) fn install_workspace(&mut self, workspace: TransactionWorkspace) {
        self.staged_globals = workspace.staged_globals;
        self.staged_granules = workspace.staged_granules;
        self.staged_memory_sizes = workspace.staged_memory_sizes;
        self.staged_table_sizes = workspace.staged_table_sizes;
        self.staged_table_elements = workspace.staged_table_elements;
        self.staged_objects = workspace.staged_objects;
        self.pending_persistent_root_versions = workspace.pending_persistent_root_versions;
        self.promoted_objects = workspace.promoted_objects;
        self.promoted_gc_refs = workspace.promoted_gc_refs;
        self.durable_leaf_gc_refs = workspace.durable_leaf_gc_refs;
        self.allocated_objects = workspace.allocated_objects;
        self.active_conflict_aborted = workspace.conflict_aborted;
        self.read_granules = workspace.read_granules;
        self.write_granules = workspace.write_granules;
        self.uncommitted_publication_streams = workspace.uncommitted_publication_streams;
        self.pending_linear_undo_chunks = workspace.pending_linear_undo_chunks;
        self.scratch = workspace.scratch;
        self.pending_memory_store = workspace.pending_memory_store;
    }

    pub(super) fn clear_active(&mut self) -> Result<()> {
        let mut error = None;
        if let Some(transaction) = self.active {
            if self.terminal_commit_active {
                if let Some(runtime) = &self.shared_region_runtime {
                    if let Err(err) = runtime.end_terminal_commit(transaction) {
                        if error.is_none() {
                            error = Some(err);
                        }
                    }
                }
            }
            if let Err(err) = self.release_transaction_authority(transaction) {
                if error.is_none() {
                    error = Some(err);
                }
            }
            if let Some(runtime) = &self.shared_region_runtime {
                let release_result = if self.uncommitted_publication_streams.is_empty() {
                    runtime.release_current_thread_log_segment_reusable()
                } else {
                    runtime.retire_current_thread_log_segment()
                };
                if let Err(err) = release_result {
                    if error.is_none() {
                        error = Some(err);
                    }
                }
            }
        }
        self.active = None;
        self.terminal_commit_active = false;
        replace_current_thread_transaction(None);
        self.install_workspace(TransactionWorkspace::default());
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(super) fn retry_post_commit_linear_undo_retirement(&mut self) {
        let streams = self
            .post_commit_linear_undo_chunks
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for stream_id in streams {
            self.retry_post_commit_linear_undo_retirement_for_stream(stream_id);
        }
    }

    pub(super) fn retry_post_commit_linear_undo_retirement_for_stream(&mut self, stream_id: u32) {
        let Some(chunk_starts) = self.post_commit_linear_undo_chunks.get(&stream_id).cloned()
        else {
            return;
        };
        if self
            .durable_log
            .retire_committed_linear_undo_chunks(chunk_starts)
            .is_ok()
        {
            self.post_commit_linear_undo_chunks.remove(&stream_id);
        }
    }
}

fn persistent_root_object_id_for_global_snapshot(
    object_table: &ObjectTable,
    value: GlobalSnapshot,
) -> Result<Option<ObjectId>> {
    match value {
        GlobalSnapshot::GcRef(gc_ref) => {
            object_table.persistent_object_id_for_live_bridge_or_promotion_required(gc_ref)
        }
        _ => Ok(None),
    }
}

fn persistent_root_object_id_for_table_element_snapshot(
    object_table: &ObjectTable,
    value: TableElementSnapshot,
) -> Result<Option<ObjectId>> {
    match value {
        TableElementSnapshot::GcRef(gc_ref) => {
            object_table.persistent_object_id_for_live_bridge_or_promotion_required(gc_ref)
        }
        TableElementSnapshot::FuncRef(_) => Ok(None),
    }
}

#[cfg(test)]
impl TransactionState {
    pub(super) fn new_for_test(transaction: TransactionId) -> Self {
        replace_current_thread_transaction(Some(transaction));
        Self {
            active: Some(transaction),
            next_id: transaction.as_raw().saturating_add(1),
            ..Self::default()
        }
    }

    pub(super) fn new_for_test_with_durable_log(
        transaction: TransactionId,
        durable_log: TxDurableLog,
    ) -> Self {
        let mut state = Self::new_for_test(transaction);
        state.durable_log = durable_log;
        state
    }

    pub(super) fn stage_granule_for_test(&mut self, granule: GranuleId, bytes: Vec<u8>) {
        self.staged_granules.insert(granule, bytes);
    }

    pub(super) fn staged_persistent_root_delta_for_test(
        &self,
        objects: &ObjectTable,
    ) -> Result<PersistentRootDelta> {
        self.staged_persistent_root_delta(objects)
    }

    pub(super) fn apply_committed_persistent_root_delta_for_test(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        self.apply_committed_persistent_root_delta(delta)
    }

    pub(super) fn complete_commit_with_persistent_root_delta_for_test(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        self.complete_commit_with_persistent_root_delta(delta)
    }

    pub(super) fn finish_committed_cleanup_after_durable_commit_error_for_test(
        &mut self,
    ) -> Result<()> {
        self.finish_committed_cleanup_after_durable_commit_error()
    }

    pub(super) fn persistent_root_ids_for_test(&self) -> BTreeSet<ObjectId> {
        self.persistent_root_ids().unwrap()
    }

    pub(super) fn persistent_mark_sweep_collect_for_test(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport> {
        self.persistent_mark_sweep_collect(objects)
    }

    pub(super) fn persistent_gc_maintenance_step_for_test(
        &mut self,
        object_table: &ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        self.persistent_gc_maintenance_step(object_table, budget)
    }

    pub(super) fn finish_persistent_gc_cycle_and_sweep_for_test(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>> {
        self.finish_persistent_gc_cycle_and_sweep(objects)
    }

    pub(super) fn compact_persistent_object_chunks_for_test(
        &mut self,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport> {
        self.compact_persistent_object_chunks(objects, recovery_report)
    }

    pub(super) fn promote_transaction_object_graph_for_test(
        &mut self,
        object_table: &mut ObjectTable,
        source: ObjectId,
    ) -> Result<ObjectId> {
        self.promote_transaction_object_graph(object_table, source)
    }

    pub(super) fn promoted_object_for_test(&self, source: ObjectId) -> Option<ObjectId> {
        self.promoted_objects.get(&source).copied()
    }

    pub(super) fn promoted_gc_ref_for_test(&self, gc_ref: u32) -> Option<ObjectId> {
        self.promoted_gc_refs.get(&gc_ref).copied()
    }

    pub(super) fn staged_object_payload_for_test(
        &self,
        object_id: ObjectId,
    ) -> Option<&ObjectPayload> {
        self.staged_objects.get(&object_id)
    }

    pub(super) fn allocated_object_count_for_test(&self) -> usize {
        self.allocated_objects.len()
    }

    pub(super) fn read_tmemory_range_for_test(
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

    pub(super) fn stage_tmemory_write_for_test(
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

    pub(super) fn commit_tmemory_for_test(&mut self, tmemory: &mut TMemory) -> Result<bool> {
        self.commit_tmemory_owned(Some(InstanceId::from_u32(0)), 0, tmemory)
    }

    pub(super) fn begin_terminal_commit_with_cleanup_for_test(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        self.begin_terminal_commit_with_object_cleanup(object_table)
    }
}
