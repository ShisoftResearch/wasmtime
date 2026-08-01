use crate::runtime::transaction::concurrency::TransactionConflictAction;
use alloc::sync::Arc;
use wasmtime_environ::VMSharedTypeIndex;

use super::object_table::default_type_layout_id_for_kind;
#[cfg(feature = "transaction-mvcc")]
use super::visibility::CommitTimestamp;
use super::visibility::{
    SelectedTransactionVisibility, SelectedVisibilitySnapshot, TransactionVisibility,
    VisibilityReadContext,
};
use super::*;

const TRANSACTION_LOCAL_OBJECT_ID_BASE: u64 = 1u64 << 63;

#[derive(Debug)]
pub(crate) struct TransactionState {
    pub(super) active: Option<TransactionId>,
    pub(super) failed: bool,
    pub(super) failure_code: u32,
    pub(super) fail_next_commit_before_lp_for_test: bool,
    pub(super) persistent_gc_state: Option<PersistentGcState>,
    pub(super) persistent_gc_policy: Arc<dyn TransactionPersistentGc>,
    pub(super) persistent_roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
    pub(super) persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    pub(super) next_id: u64,
    pub(super) shared_region_runtime: Option<TransactionRegionRuntime>,
    pub(super) terminal_commit_active: bool,
    pub(super) active_conflict_aborted: bool,
    pub(super) concurrency: ConcurrencyControlState,
    /// Stable visibility runtime for local transactions owned by this state.
    pub(super) visibility: SelectedTransactionVisibility,
    /// Visibility runtime paired with the active workspace snapshot.
    pub(super) selected_visibility: Option<SelectedTransactionVisibility>,
    pub(super) visibility_snapshot: Option<SelectedVisibilitySnapshot>,
    pub(super) suspended: BTreeMap<TransactionId, TransactionWorkspace>,
    pub(super) staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    pub(super) staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    pub(super) staged_memory_sizes: BTreeMap<GranuleId, u64>,
    pub(super) staged_table_sizes: BTreeMap<GranuleId, u64>,
    pub(super) staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    pub(super) staged_objects: BTreeMap<ObjectId, StagedObjectRecord>,
    pub(super) pending_persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    pub(super) promoted_objects: BTreeMap<ObjectId, ObjectId>,
    pub(super) promoted_gc_refs: BTreeMap<u32, ObjectId>,
    pub(super) durable_leaf_gc_refs: BTreeSet<u32>,
    pub(super) local_object_table: TransactionLocalObjectTable,
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
    #[cfg(feature = "transaction-mvcc")]
    pub(super) mvcc_terminal_commit: Option<MvccTerminalCommitState>,
    #[cfg(test)]
    pub(super) benchmark_cleanup_failed: bool,
    #[cfg(all(
        test,
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(super) benchmark_committed_mvcc_versions_after_error: u64,
    #[cfg(all(
        test,
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(super) benchmark_mvcc_terminal_commit: Option<BenchmarkMvccTerminalCommitState>,
}

#[derive(Debug, Default)]
pub(super) struct TransactionWorkspace {
    visibility: Option<SelectedTransactionVisibility>,
    visibility_snapshot: Option<SelectedVisibilitySnapshot>,
    staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    staged_memory_sizes: BTreeMap<GranuleId, u64>,
    staged_table_sizes: BTreeMap<GranuleId, u64>,
    staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    staged_objects: BTreeMap<ObjectId, StagedObjectRecord>,
    pending_persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
    promoted_objects: BTreeMap<ObjectId, ObjectId>,
    promoted_gc_refs: BTreeMap<u32, ObjectId>,
    durable_leaf_gc_refs: BTreeSet<u32>,
    local_object_table: TransactionLocalObjectTable,
    allocated_objects: Vec<ObjectId>,
    conflict_aborted: bool,
    read_granules: BTreeSet<GranuleId>,
    write_granules: BTreeSet<GranuleId>,
    uncommitted_publication_streams: BTreeSet<u32>,
    pending_linear_undo_chunks: BTreeMap<u32, BTreeSet<u32>>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
    #[cfg(feature = "transaction-mvcc")]
    mvcc_terminal_commit: Option<MvccTerminalCommitState>,
    #[cfg(all(
        test,
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    benchmark_mvcc_terminal_commit: Option<BenchmarkMvccTerminalCommitState>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            failed: false,
            failure_code: 0,
            fail_next_commit_before_lp_for_test: false,
            persistent_gc_state: None,
            persistent_gc_policy: Arc::new(CurrentStatePersistentGc),
            persistent_roots: BTreeMap::new(),
            persistent_root_versions: BTreeMap::new(),
            next_id: 10_001,
            shared_region_runtime: None,
            terminal_commit_active: false,
            active_conflict_aborted: false,
            concurrency: ConcurrencyControlState::default(),
            visibility: SelectedTransactionVisibility::default(),
            selected_visibility: None,
            visibility_snapshot: None,
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
            local_object_table: TransactionLocalObjectTable::default(),
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
            #[cfg(feature = "transaction-mvcc")]
            mvcc_terminal_commit: None,
            #[cfg(test)]
            benchmark_cleanup_failed: false,
            #[cfg(all(
                test,
                feature = "transaction-mvcc",
                feature = "transaction-cc-optimistic-validation"
            ))]
            benchmark_committed_mvcc_versions_after_error: 0,
            #[cfg(all(
                test,
                feature = "transaction-mvcc",
                feature = "transaction-cc-optimistic-validation"
            ))]
            benchmark_mvcc_terminal_commit: None,
        }
    }
}

#[cfg(feature = "transaction-mvcc")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MvccTerminalDecision {
    Abortable,
    Rollback,
    ForceCommit,
}

#[cfg(feature = "transaction-mvcc")]
#[derive(Debug)]
pub(crate) struct MvccTerminalCommitState {
    pub(crate) instance: InstanceId,
    pub(crate) stream_id: u32,
    pub(crate) txid: u32,
    pub(crate) commit: Arc<mvcc::CommitRecord>,
    pub(crate) pending: Option<mvcc::PendingCommitRegistration>,
    pub(crate) certification: Option<concurrency::MvccCertificationPermit>,
    pub(crate) user_region_permit: Option<region_runtime::UserTransactionRegionPermit>,
    pub(crate) prepared: PreparedDomainValues,
    pub(crate) installed: InstalledDomainKeys,
    pub(crate) undo_published: BTreeSet<GranuleId>,
    pub(crate) durable_publications: Vec<persist::PendingPublication>,
    pub(crate) durable_publication_markers: Vec<PendingCommitLogEntry>,
    pub(crate) durable_publications_installed: BTreeSet<usize>,
    pub(crate) root_delta: PersistentRootDelta,
    pub(crate) persistent_gc_delta: PersistentGcCommitDelta,
    pub(crate) volatile_objects: Vec<(ObjectId, ObjectPayload)>,
    pub(crate) final_marker: Option<PendingCommitLogEntry>,
    pub(crate) lp_published: bool,
    pub(crate) decision: MvccTerminalDecision,
    pub(crate) root_applied: bool,
    pub(crate) version_bumps_applied: bool,
    pub(crate) completion_error: Option<String>,
    #[cfg(test)]
    pub(crate) inside_install_hook_ran: bool,
    #[cfg(test)]
    pub(crate) before_publish_hook_ran: bool,
    #[cfg(test)]
    pub(crate) after_publish_hook_ran: bool,
}

#[cfg(all(
    test,
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
#[derive(Debug)]
pub(super) struct BenchmarkMvccTerminalCommitState {
    visibility: Arc<mvcc::MvccRuntime>,
    commit: Arc<mvcc::CommitRecord>,
    pending: Option<mvcc::PendingCommitRegistration>,
    certification: Option<concurrency::MvccCertificationPermit>,
    user_region_permit: Option<region_runtime::UserTransactionRegionPermit>,
    prepared: PreparedDomainValues,
    installed: BTreeSet<GranuleId>,
    undo_published: BTreeSet<GranuleId>,
    stream_id: u32,
    txid: u32,
    final_marker: Option<PendingCommitLogEntry>,
    lp_published: bool,
    version_bumps_applied: bool,
    wrote: bool,
    decision: MvccTerminalDecision,
    completion_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TransactionLocalObjectSlot {
    pub(super) kind: ObjectKind,
    pub(super) type_layout_id: u32,
    pub(super) runtime_type_index: Option<VMSharedTypeIndex>,
    pub(super) payload: ObjectPayload,
}

#[derive(Clone, Debug)]
pub(crate) struct TransactionLocalObjectTable {
    slots: BTreeMap<ObjectId, TransactionLocalObjectSlot>,
    transaction_ref_handles_to_objects: BTreeMap<u32, ObjectId>,
    objects_to_transaction_ref_handles: BTreeMap<ObjectId, u32>,
    next_object_index: u64,
}

impl Default for TransactionLocalObjectTable {
    fn default() -> Self {
        Self {
            slots: BTreeMap::new(),
            transaction_ref_handles_to_objects: BTreeMap::new(),
            objects_to_transaction_ref_handles: BTreeMap::new(),
            next_object_index: TRANSACTION_LOCAL_OBJECT_ID_BASE,
        }
    }
}

impl VisibilityReadContext {
    pub(crate) fn read_memory(
        &self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        self.visibility.read_memory(self.snapshot, granule, current)
    }

    pub(crate) fn read_memory_size(
        &self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
    ) -> Result<u64> {
        self.visibility
            .read_memory_size(self.snapshot, granule, current)
    }

    pub(crate) fn read_global(
        &self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<GlobalSnapshot>,
    ) -> Result<GlobalSnapshot> {
        self.visibility.read_global(self.snapshot, granule, current)
    }

    pub(crate) fn read_table(
        &self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<TableGranuleSnapshot>,
    ) -> Result<TableGranuleSnapshot> {
        self.visibility.read_table(self.snapshot, granule, current)
    }

    pub(crate) fn read_table_size(
        &self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
    ) -> Result<u64> {
        self.visibility
            .read_table_size(self.snapshot, granule, current)
    }

    pub(crate) fn read_object(
        &self,
        object_id: ObjectId,
        current: impl FnOnce() -> Result<Option<ObjectPayload>>,
    ) -> Result<Option<ObjectPayload>> {
        self.visibility
            .read_object(self.snapshot, object_id, current)
    }
}

impl TransactionLocalObjectTable {
    fn is_local_object_id(object_id: ObjectId) -> bool {
        object_id.object_index >= TRANSACTION_LOCAL_OBJECT_ID_BASE
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    fn allocate_payload(
        &mut self,
        payload: ObjectPayload,
        runtime_type_index: Option<VMSharedTypeIndex>,
    ) -> Result<ObjectId> {
        let object_id = ObjectId {
            object_index: self.next_object_index,
        };
        self.next_object_index = self
            .next_object_index
            .checked_add(1)
            .context("transaction-local object id overflow")?;
        ensure!(
            Self::is_local_object_id(object_id),
            "transaction-local object id namespace overflow"
        );
        let kind = payload.kind();
        let type_layout_id = default_type_layout_id_for_kind(kind).get();
        let previous = self.slots.insert(
            object_id,
            TransactionLocalObjectSlot {
                kind,
                type_layout_id,
                runtime_type_index,
                payload,
            },
        );
        ensure!(
            previous.is_none(),
            "transaction-local object id was allocated twice"
        );
        Ok(object_id)
    }

    fn allocate_struct(
        &mut self,
        fields: Vec<ObjectValue>,
        runtime_type_index: Option<VMSharedTypeIndex>,
    ) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Struct(fields), runtime_type_index)
    }

    fn allocate_array(
        &mut self,
        elements: Vec<ObjectValue>,
        runtime_type_index: Option<VMSharedTypeIndex>,
    ) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Array(elements), runtime_type_index)
    }

    pub(super) fn slot(&self, object_id: ObjectId) -> Option<&TransactionLocalObjectSlot> {
        self.slots.get(&object_id)
    }

    pub(super) fn contains(&self, object_id: ObjectId) -> bool {
        self.slots.contains_key(&object_id)
    }

    fn kind(&self, object_id: ObjectId) -> Result<ObjectKind> {
        self.slot(object_id).map(|slot| slot.kind).with_context(|| {
            format!("transaction-local object table slot is not live: {object_id:?}")
        })
    }

    fn payload(&self, object_id: ObjectId) -> Result<ObjectPayload> {
        self.slot(object_id)
            .map(|slot| slot.payload.clone())
            .with_context(|| {
                format!("transaction-local object table slot is not live: {object_id:?}")
            })
    }

    fn runtime_type_index(&self, object_id: ObjectId) -> Result<Option<VMSharedTypeIndex>> {
        self.slot(object_id)
            .map(|slot| slot.runtime_type_index)
            .with_context(|| {
                format!("transaction-local object table slot is not live: {object_id:?}")
            })
    }

    fn known_transaction_ref_handle_for_object_id(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<u32>> {
        self.kind(object_id)?;
        Ok(self
            .objects_to_transaction_ref_handles
            .get(&object_id)
            .copied())
    }

    fn associate_transaction_ref_handle_for_object_id(
        &mut self,
        handle: u32,
        object_id: ObjectId,
    ) -> Result<()> {
        let handle = TransactionObjectRefRaw::from_handle(handle)?.as_raw();
        self.kind(object_id)?;
        if let Some(existing) = self
            .transaction_ref_handles_to_objects
            .get(&handle)
            .copied()
        {
            ensure!(
                existing == object_id,
                "transaction object ref handle is already associated"
            );
        }
        if let Some(existing) = self
            .objects_to_transaction_ref_handles
            .get(&object_id)
            .copied()
        {
            ensure!(
                existing == handle,
                "transaction object id is already associated with another ref handle"
            );
        }
        self.transaction_ref_handles_to_objects
            .insert(handle, object_id);
        self.objects_to_transaction_ref_handles
            .insert(object_id, handle);
        Ok(())
    }

    fn known_object_id_for_transaction_ref_handle(&self, handle: u32) -> Option<ObjectId> {
        let handle = TransactionObjectRefRaw::from_raw(handle).decode()?;
        let object_id = self
            .transaction_ref_handles_to_objects
            .get(&handle)
            .copied()?;
        self.contains(object_id).then_some(object_id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StagedObjectRecord {
    payload: ObjectPayload,
}

impl StagedObjectRecord {
    pub(crate) fn new(payload: ObjectPayload) -> Self {
        Self { payload }
    }

    pub(crate) fn payload(&self) -> &ObjectPayload {
        &self.payload
    }

    pub(crate) fn into_payload(self) -> ObjectPayload {
        self.payload
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
        let visibility = self.visibility.clone();
        let snapshot = visibility.begin_snapshot()?;
        let id = match self.allocate_local_transaction_id() {
            Ok(id) => id,
            Err(error) => {
                return Self::finish_snapshot_after_error(&visibility, snapshot, error);
            }
        };
        self.selected_visibility = Some(visibility);
        self.visibility_snapshot = Some(snapshot);
        self.activate_transaction(id);
        Ok(id)
    }

    pub(crate) fn begin_with_region_runtime(
        &mut self,
        region: &TransactionRegionRuntime,
    ) -> Result<TransactionId> {
        self.prepare_to_begin_transaction()?;
        let visibility = region.visibility();
        let snapshot = visibility.begin_snapshot()?;
        let id = match region.allocate_transaction_id() {
            Ok(id) => id,
            Err(error) => {
                return Self::finish_snapshot_after_error(&visibility, snapshot, error);
            }
        };
        self.shared_region_runtime = Some(region.clone());
        self.selected_visibility = Some(visibility);
        self.visibility_snapshot = Some(snapshot);
        self.activate_transaction(id);
        Ok(id)
    }

    fn finish_snapshot_after_error<T>(
        visibility: &SelectedTransactionVisibility,
        snapshot: SelectedVisibilitySnapshot,
        error: crate::Error,
    ) -> Result<T> {
        Self::combine_results(
            Err(error),
            visibility.finish_snapshot(snapshot),
            "failed to finish transaction visibility snapshot",
        )
    }

    fn combine_results<T>(
        primary: Result<T>,
        cleanup: Result<()>,
        cleanup_context: &str,
    ) -> Result<T> {
        combine_operation_and_cleanup_results(primary, cleanup, cleanup_context)
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
        let workspace = match self.suspended.remove(&transaction) {
            Some(workspace) => workspace,
            None => {
                let visibility = self.visibility_for_new_workspace();
                let snapshot = match visibility.begin_snapshot() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        self.restore_after_failed_enter(previous)?;
                        return Err(error);
                    }
                };
                TransactionWorkspace {
                    visibility: Some(visibility),
                    visibility_snapshot: Some(snapshot),
                    ..TransactionWorkspace::default()
                }
            }
        };
        self.install_workspace(workspace);
        self.active = Some(transaction);
        replace_current_thread_transaction(Some(transaction));
        Ok(previous)
    }

    fn restore_after_failed_enter(&mut self, previous: Option<TransactionId>) -> Result<()> {
        match previous {
            Some(transaction) => {
                let workspace = self
                    .suspended
                    .remove(&transaction)
                    .expect("just-suspended transaction workspace must remain installed");
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
            let mut workspace = workspace;
            let mut result = self.bump_versioned_granules(workspace.write_granules.iter().copied());
            result = Self::combine_results(
                result,
                self.release_transaction_authority(transaction),
                "failed to release suspended transaction authority",
            );
            return Self::finish_workspace_snapshot(&mut workspace, result).map(|()| true);
        }
        Ok(false)
    }

    pub(crate) fn abort_transaction_allocated_objects(
        &mut self,
        object_table: &mut ObjectTable,
        transaction: TransactionId,
    ) -> Result<bool> {
        if self.active == Some(transaction) {
            return self.abort_allocated_objects(object_table).map(|()| true);
        }
        self.retry_post_commit_linear_undo_retirement();
        let mut result = self
            .drain_conflict_aborted_allocated_objects(object_table)
            .map(|_| ());
        let Some(mut workspace) = self.suspended.remove(&transaction) else {
            return result.map(|()| false);
        };
        let allocated = workspace
            .allocated_objects
            .iter()
            .rev()
            .copied()
            .collect::<Vec<_>>();
        for (index, object_id) in allocated.iter().copied().enumerate() {
            if let Err(error) = object_table.free(object_id) {
                self.pending_conflict_aborted_allocated_objects
                    .extend(allocated[index..].iter().copied());
                result = Self::combine_results(
                    result,
                    Err(error),
                    "failed to free a suspended transaction object",
                );
                break;
            }
        }
        result = Self::combine_results(
            result,
            self.bump_versioned_granules(workspace.write_granules.iter().copied()),
            "failed to bump suspended transaction granule versions",
        );
        result = Self::combine_results(
            result,
            self.release_transaction_authority(transaction),
            "failed to release suspended transaction authority",
        );
        Self::finish_workspace_snapshot(&mut workspace, result).map(|()| true)
    }

    pub(crate) fn active_visibility_read_context(&self) -> Result<VisibilityReadContext> {
        self.ensure_active()?;
        let snapshot = self
            .visibility_snapshot
            .as_ref()
            .context("active transaction is missing a visibility snapshot")?;
        let visibility = self
            .selected_visibility
            .as_ref()
            .context("active transaction is missing its selected visibility runtime")?;
        Ok(VisibilityReadContext {
            visibility: visibility.clone(),
            snapshot: SelectedTransactionVisibility::snapshot_timestamp(snapshot),
        })
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn active_mvcc_commit_context(
        &self,
    ) -> Result<(
        Arc<mvcc::MvccRuntime>,
        CommitTimestamp,
        TransactionId,
        BTreeSet<GranuleId>,
        BTreeSet<GranuleId>,
    )> {
        let transaction = self.active_transaction_required()?;
        let context = self.active_visibility_read_context()?;
        Ok((
            context.visibility.runtime().clone(),
            context.snapshot,
            transaction,
            self.read_granules.clone(),
            self.write_granules.clone(),
        ))
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn acquire_active_mvcc_certification(
        &self,
        runtime: &Arc<mvcc::MvccRuntime>,
        snapshot: CommitTimestamp,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<concurrency::MvccCertificationPermit> {
        if let Some(region) = &self.shared_region_runtime {
            return region.acquire_mvcc_certification(
                transaction,
                reads,
                writes,
                snapshot,
                runtime,
            );
        }
        let permit = self
            .concurrency
            .acquire_mvcc_certification(transaction, reads, writes)?;
        for granule in reads.union(writes).copied() {
            ensure!(
                runtime.latest_committed_timestamp(granule)? <= snapshot,
                "transaction MVCC certification conflict on {granule:?}"
            );
        }
        Ok(permit)
    }

    fn visibility_for_new_workspace(&self) -> SelectedTransactionVisibility {
        self.shared_region_runtime
            .as_ref()
            .map(TransactionRegionRuntime::visibility)
            .unwrap_or_else(|| self.visibility.clone())
    }

    fn finish_workspace_snapshot(
        workspace: &mut TransactionWorkspace,
        result: Result<()>,
    ) -> Result<()> {
        let visibility = workspace.visibility.take();
        let snapshot = workspace.visibility_snapshot.take();
        let finish = match (visibility, snapshot) {
            (Some(visibility), Some(snapshot)) => visibility.finish_snapshot(snapshot),
            (None, None) => Ok(()),
            (Some(_), None) => Err(crate::format_err!(
                "transaction workspace visibility runtime is missing its snapshot"
            )),
            (None, Some(_)) => Err(crate::format_err!(
                "transaction workspace snapshot is missing its visibility runtime"
            )),
        };
        Self::combine_results(
            result,
            finish,
            "failed to finish transaction visibility snapshot",
        )
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

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn install_mvcc_terminal_commit(
        &mut self,
        terminal: MvccTerminalCommitState,
    ) -> Result<()> {
        self.ensure_active()?;
        ensure!(
            self.mvcc_terminal_commit.replace(terminal).is_none(),
            "MVCC terminal commit state is already installed"
        );
        Ok(())
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn take_mvcc_terminal_commit(&mut self) -> Option<MvccTerminalCommitState> {
        self.mvcc_terminal_commit.take()
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn has_mvcc_terminal_commit(&self) -> bool {
        self.mvcc_terminal_commit.is_some()
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
        Ok(self
            .publish_object_publications_before_commit_with_markers(
                stream_id,
                txid,
                object_table,
                publications,
            )?
            .last()
            .copied())
    }

    pub(crate) fn publish_object_publications_before_commit_with_markers(
        &mut self,
        stream_id: u32,
        txid: u32,
        object_table: &ObjectTable,
        publications: &[persist::PendingPublication],
    ) -> Result<Vec<PendingCommitLogEntry>> {
        if publications.is_empty() {
            return Ok(Vec::new());
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
        publisher.publish_object_publications_before_commit_with_markers(publications)
    }

    pub(crate) fn install_committed_mapped_object_publications(
        &self,
        object_table: &mut ObjectTable,
        publications: &[persist::PendingPublication],
        markers: &[PendingCommitLogEntry],
    ) -> Result<Vec<ObjectId>> {
        ensure!(
            publications.len() == markers.len(),
            "committed publication marker count does not match publication count"
        );
        let mut has_object_publication = false;
        for publication in publications {
            if publication.persistent_object_type_layout_id()?.is_some() {
                has_object_publication = true;
                break;
            }
        }
        if !has_object_publication {
            return Ok(Vec::new());
        }
        let Some(mapped_source) = self.durable_log.cloned_mapped_region_source() else {
            return object_table.install_committed_serialized_persistent_publications(publications);
        };
        let Some(runtime) = &self.shared_region_runtime else {
            return object_table.install_committed_mapped_persistent_publications(
                publications,
                markers,
                mapped_source,
            );
        };
        let candidates = object_table.committed_mapped_persistent_publication_entries(
            publications,
            markers,
            mapped_source,
        )?;
        for candidate in &candidates {
            object_table
                .validate_persistent_object_directory_entry_source(&candidate.directory_entry)?;
        }
        let installed = runtime.install_persistent_object_directory_entries(
            candidates.into_iter().map(|candidate| {
                debug_assert_eq!(candidate.object_id, candidate.directory_entry.object_id);
                candidate.directory_entry
            }),
        )?;
        let mut installed_ids = Vec::with_capacity(installed.len());
        for entry in installed {
            ensure!(
                entry.record_source.is_some(),
                "shared persistent object directory entry has no mapped record source"
            );
            installed_ids.push(entry.object_id);
            object_table.install_persistent_object_directory_entry(entry)?;
        }
        Ok(installed_ids)
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
        #[cfg(feature = "transaction-mvcc")]
        {
            let _ = current_version;
            self.ensure_active()?;
            return Ok(self.read_granules.insert(granule));
        }
        #[cfg(not(feature = "transaction-mvcc"))]
        {
            self.ensure_active()?;
            let transaction = self.active_transaction_required()?;
            let current_version = self.current_version_for_granule(granule, current_version)?;
            let action =
                self.acquire_granule_read_authority(transaction, granule, current_version)?;
            let refresh_after_abort = action.aborted_transaction().is_some();
            self.handle_conflict_action(action)?;
            if refresh_after_abort {
                let current_version = self.current_version_for_granule(granule, current_version)?;
                self.refresh_read_version_authority(transaction, granule, current_version)?;
            }
            Ok(self.read_granules.insert(granule))
        }
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<bool> {
        #[cfg(feature = "transaction-mvcc")]
        {
            let _ = current_version;
            self.ensure_active()?;
            self.read_granules.insert(granule);
            return Ok(self.write_granules.insert(granule));
        }
        #[cfg(not(feature = "transaction-mvcc"))]
        {
            self.ensure_active()?;
            let transaction = self.active_transaction_required()?;
            let current_version = self.current_version_for_granule(granule, current_version)?;
            let action =
                self.acquire_granule_write_authority(transaction, granule, current_version)?;
            let refresh_after_abort = action.aborted_transaction().is_some();
            self.handle_conflict_action(action)?;
            if refresh_after_abort {
                let current_version = self.current_version_for_granule(granule, current_version)?;
                self.refresh_read_version_authority(transaction, granule, current_version)?;
            }
            self.read_granules.insert(granule);
            Ok(self.write_granules.insert(granule))
        }
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
        #[cfg(feature = "transaction-mvcc")]
        let _ = current_version_fn;
        #[cfg(not(feature = "transaction-mvcc"))]
        let mut current_version_fn = current_version_fn;
        #[cfg(not(feature = "transaction-mvcc"))]
        self.validate_active_read_granules_with(&mut current_version_fn)?;
        #[cfg(not(feature = "transaction-mvcc"))]
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
        #[cfg(feature = "transaction-mvcc")]
        {
            let _ = current_version_fn;
            self.ensure_active()?;
            return Ok(());
        }
        #[cfg(not(feature = "transaction-mvcc"))]
        {
            let mut current_version_fn = current_version_fn;
            self.validate_active_read_granules_with(&mut current_version_fn)
        }
    }

    pub(crate) fn active_read_granules(&self) -> Result<Vec<GranuleId>> {
        let transaction = self.active_transaction_required()?;
        #[cfg(feature = "transaction-mvcc")]
        {
            let _ = transaction;
            return Ok(self.read_granules.iter().copied().collect());
        }
        #[cfg(not(feature = "transaction-mvcc"))]
        {
            if self.shared_region_runtime.is_some() {
                return Ok(self.read_granules.iter().copied().collect());
            }
            Ok(self.concurrency.read_granules_for_transaction(transaction))
        }
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
        #[cfg(feature = "transaction-mvcc")]
        {
            let _ = (granule, current_version);
            self.ensure_active()?;
            return Ok(());
        }
        #[cfg(not(feature = "transaction-mvcc"))]
        {
            let transaction = self.active_transaction_required()?;
            let current_version = self.current_version_for_granule(granule, current_version)?;
            self.validate_granule_read_authority(transaction, granule, current_version)
        }
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

    fn current_object_version_for_granule(
        &self,
        granule: GranuleId,
        object_table: &ObjectTable,
    ) -> Result<u64> {
        let Some(object_id) = object_granule_object_id(granule) else {
            return Ok(0);
        };
        if let Some(runtime) = &self.shared_region_runtime {
            return runtime.persistent_object_directory_version(object_id);
        }
        object_table.version(object_id)
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
            return Self::combine_results(
                Err(crate::format_err!(
                    "transaction was conflict-aborted by another transaction"
                )),
                self.finish_active_conflict_aborted_without_object_cleanup(),
                "failed to clean up conflict-aborted transaction",
            );
        }
        bail!(
            "transaction was conflict-aborted by another transaction and requires object-aware cleanup for allocated objects"
        )
    }

    fn fail_active_conflict_aborted_with_object_cleanup(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        Self::combine_results(
            Err(crate::format_err!(
                "transaction was conflict-aborted by another transaction"
            )),
            self.abort_allocated_objects(object_table),
            "failed to clean up conflict-aborted transaction",
        )
    }

    fn finish_active_conflict_aborted_without_object_cleanup(&mut self) -> Result<()> {
        self.retry_post_commit_linear_undo_retirement();
        let result = self.bump_active_versioned_write_granules();
        Self::combine_results(
            result,
            self.clear_active(),
            "failed to clear conflict-aborted transaction",
        )
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
        if let Some(mut workspace) = self.suspended.remove(&transaction) {
            let mut result = self
                .take_shared_conflict_aborted_transaction(transaction)
                .map(|_| ());
            self.pending_conflict_aborted_allocated_objects
                .extend(workspace.allocated_objects.iter().rev().copied());
            result = Self::combine_results(
                result,
                self.bump_versioned_granules(workspace.write_granules.iter().copied()),
                "failed to bump conflict-aborted transaction granule versions",
            );
            Self::finish_workspace_snapshot(&mut workspace, result)?;
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
        let updates = granules
            .into_iter()
            .filter(|granule| granule_uses_transaction_state_version(*granule))
            .map(|granule| {
                let version = self
                    .granule_versions
                    .get(&granule)
                    .copied()
                    .unwrap_or(0)
                    .checked_add(1)
                    .context("transaction granule version overflow")?;
                Ok((granule, version))
            })
            .collect::<Result<Vec<_>>>()?;
        for (granule, version) in updates {
            self.granule_versions.insert(granule, version);
        }
        Ok(())
    }

    pub(crate) fn complete_commit(&mut self) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        self.bump_active_versioned_write_granules()?;
        self.finish_commit_after_policy_hook(transaction)
    }

    pub(crate) fn complete_commit_with_persistent_root_delta(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        if self.shared_region_runtime.is_some() {
            self.commit_shared_persistent_root_delta_before_complete_commit(delta)?;
            self.bump_active_versioned_write_granules()?;
            self.finish_commit_after_policy_hook(transaction)?;
            return Ok(());
        }
        self.bump_active_versioned_write_granules()?;
        self.finish_commit_after_policy_hook(transaction)?;
        self.apply_committed_persistent_root_delta(delta)?;
        Ok(())
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn prepare_mvcc_root_commit_completion(
        &mut self,
        delta: PersistentRootDelta,
    ) -> Result<()> {
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.retry_post_commit_linear_undo_retirement();
        if self.shared_region_runtime.is_some() {
            self.commit_shared_persistent_root_delta_before_complete_commit(delta)?;
        } else {
            self.apply_committed_persistent_root_delta(delta)?;
        }
        Ok(())
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn prepare_mvcc_version_commit_completion(&mut self) -> Result<()> {
        self.begin_terminal_commit()?;
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.bump_active_versioned_write_granules()
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn finish_mvcc_commit_cleanup(&mut self) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.finish_commit_after_policy_hook(transaction)
    }

    fn finish_commit_after_policy_hook(&mut self, transaction: TransactionId) -> Result<()> {
        let result = self.commit_transaction_authority(transaction);
        Self::combine_results(
            result,
            self.clear_active(),
            "failed to clear committed transaction",
        )
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
        self.ensure_no_pending_conflict_aborted_allocated_objects()?;
        self.ensure_no_active_allocated_objects_for_generic_abort()?;
        self.active_conflict_aborted = false;
        let transaction = self.active_transaction_required()?;
        self.retry_post_commit_linear_undo_retirement();
        let mut result = self
            .take_shared_conflict_aborted_transaction(transaction)
            .map(|_| ());
        result = Self::combine_results(
            result,
            self.bump_active_versioned_write_granules(),
            "failed to bump aborted transaction granule versions",
        );
        Self::combine_results(
            result,
            self.clear_active(),
            "failed to clear aborted transaction",
        )
    }

    pub(crate) fn record_allocated_object(&mut self, object_id: ObjectId) -> Result<bool> {
        self.ensure_active()?;
        if self.allocated_objects.contains(&object_id) {
            return Ok(false);
        }
        self.allocated_objects.push(object_id);
        Ok(true)
    }

    pub(crate) fn allocate_transaction_local_struct(
        &mut self,
        fields: Vec<ObjectValue>,
        runtime_type_index: Option<VMSharedTypeIndex>,
    ) -> Result<ObjectId> {
        self.ensure_active()?;
        self.local_object_table
            .allocate_struct(fields, runtime_type_index)
    }

    pub(crate) fn allocate_transaction_local_array(
        &mut self,
        elements: Vec<ObjectValue>,
        runtime_type_index: Option<VMSharedTypeIndex>,
    ) -> Result<ObjectId> {
        self.ensure_active()?;
        self.local_object_table
            .allocate_array(elements, runtime_type_index)
    }

    pub(crate) fn transaction_ref_handle_for_object_id_avoiding<F>(
        &mut self,
        object_table: &mut ObjectTable,
        object_id: ObjectId,
        is_reserved_live_ref_raw: F,
    ) -> Result<u32>
    where
        F: Fn(u32) -> bool,
    {
        if self.local_object_table.contains(object_id) {
            if let Some(handle) = self
                .local_object_table
                .known_transaction_ref_handle_for_object_id(object_id)?
            {
                ensure!(
                    !is_reserved_live_ref_raw(handle),
                    "transaction object ref handle collides with registered live ref"
                );
                ensure!(
                    object_table
                        .known_object_id_for_transaction_ref_handle(handle)
                        .is_none(),
                    "transaction object ref handle collides with registered live ref"
                );
                ensure!(
                    object_table
                        .known_object_id_for_live_gc_ref_bridge(handle)
                        .is_none(),
                    "transaction object ref handle collides with registered live ref"
                );
                return Ok(handle);
            }

            let handle = object_table.reserve_transaction_ref_handle_avoiding(|raw| {
                is_reserved_live_ref_raw(raw)
                    || self
                        .local_object_table
                        .known_object_id_for_transaction_ref_handle(raw)
                        .is_some()
            })?;
            self.local_object_table
                .associate_transaction_ref_handle_for_object_id(handle, object_id)?;
            return Ok(handle);
        }
        object_table
            .transaction_ref_handle_for_object_id_avoiding(object_id, is_reserved_live_ref_raw)
    }

    pub(crate) fn known_object_id_for_transaction_ref_handle(
        &self,
        object_table: &ObjectTable,
        handle: u32,
    ) -> Option<ObjectId> {
        self.local_object_table
            .known_object_id_for_transaction_ref_handle(handle)
            .or_else(|| object_table.known_object_id_for_transaction_ref_handle(handle))
    }

    pub(crate) fn object_id_for_transaction_ref_handle(
        &self,
        object_table: &ObjectTable,
        handle: u32,
    ) -> Result<ObjectId> {
        if let Some(object_id) = self
            .local_object_table
            .known_object_id_for_transaction_ref_handle(handle)
        {
            return Ok(object_id);
        }
        object_table.object_id_for_transaction_ref_handle(handle)
    }

    pub(crate) fn object_id_for_live_bridge_transaction_ref_raw(
        &self,
        object_table: &ObjectTable,
        raw_ref: u32,
    ) -> Result<ObjectId> {
        ensure!(raw_ref != 0, "transactional object cannot use null ref");
        if let Some(object_id) =
            self.known_object_id_for_transaction_ref_handle(object_table, raw_ref)
        {
            return Ok(object_id);
        }
        object_table.object_id_for_live_bridge_transaction_ref_raw(raw_ref)
    }

    pub(crate) fn known_persistent_object_id_for_transaction_ref_handle(
        &self,
        object_table: &ObjectTable,
        handle: u32,
    ) -> Result<Option<ObjectId>> {
        if self
            .local_object_table
            .known_object_id_for_transaction_ref_handle(handle)
            .is_some()
        {
            return Ok(None);
        }
        object_table.known_persistent_object_id_for_transaction_ref_handle(handle)
    }

    pub(crate) fn object_kind(
        &self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<ObjectKind> {
        if self.local_object_table.contains(object_id) {
            return self.local_object_table.kind(object_id);
        }
        object_table.kind(object_id)
    }

    pub(crate) fn runtime_type_index(
        &self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<Option<VMSharedTypeIndex>> {
        if self.local_object_table.contains(object_id) {
            return self.local_object_table.runtime_type_index(object_id);
        }
        object_table.runtime_type_index(object_id)
    }

    pub(crate) fn finalize_transaction_local_objects_after_promotion(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        self.ensure_active()?;
        if self.local_object_table.is_empty() {
            return Ok(());
        }

        let local_handles = self
            .local_object_table
            .transaction_ref_handles_to_objects
            .clone();
        let local_object_ids = self
            .local_object_table
            .slots
            .keys()
            .copied()
            .collect::<Vec<_>>();

        for (handle, local_id) in local_handles {
            let Some(promoted) = self.promoted_objects.get(&local_id).copied() else {
                continue;
            };
            ensure!(
                self.allocated_objects.contains(&promoted),
                "promoted transaction-local handle target was not recorded as allocated"
            );
            ensure!(
                object_table.is_persistent(promoted)?,
                "promoted transaction-local handle target must be persistent"
            );
            object_table.associate_transaction_ref_handle_for_object_id(handle, promoted)?;
        }

        for local_id in local_object_ids {
            self.staged_objects.remove(&local_id);
        }
        self.local_object_table = TransactionLocalObjectTable::default();
        Ok(())
    }

    pub(crate) fn abort_allocated_objects(&mut self, object_table: &mut ObjectTable) -> Result<()> {
        self.ensure_active()?;
        self.active_conflict_aborted = false;
        let transaction = self.active_transaction_required()?;
        self.retry_post_commit_linear_undo_retirement();
        let mut result = self
            .take_shared_conflict_aborted_transaction(transaction)
            .map(|_| ());
        result = Self::combine_results(
            result,
            self.drain_conflict_aborted_allocated_objects(object_table)
                .map(|_| ()),
            "failed to drain conflict-aborted transaction objects",
        );
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
                result = Self::combine_results(
                    result,
                    Err(error),
                    "failed to free an aborted transaction object",
                );
                break;
            }
        }
        result = Self::combine_results(
            result,
            self.bump_active_versioned_write_granules(),
            "failed to bump aborted transaction granule versions",
        );
        Self::combine_results(
            result,
            self.clear_active(),
            "failed to clear aborted transaction",
        )
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

    pub(crate) fn pending_memory_store(
        &self,
    ) -> Option<(InstanceId, Option<InstanceId>, u32, u64, usize)> {
        self.pending_memory_store.map(|pending| {
            (
                pending.instance,
                pending.owner_instance_key,
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
        object_table.refresh_persistent_object_from_shared_directory(object_id)?;
        let granule = object_table.granule_id(object_id)?;
        let version = self.current_object_version_for_granule(granule, object_table)?;
        let acquired = self.acquire_granule_read(granule, version)?;
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        Ok(acquired)
    }

    pub(crate) fn acquire_object_write(
        &mut self,
        object_table: &mut ObjectTable,
        object_id: ObjectId,
    ) -> Result<bool> {
        object_table.refresh_persistent_object_from_shared_directory(object_id)?;
        let granule = object_table.granule_id(object_id)?;
        let version = self.current_object_version_for_granule(granule, object_table)?;
        let acquired = self.acquire_granule_write(granule, version)?;
        self.drain_conflict_aborted_allocated_objects(object_table)?;
        Ok(acquired)
    }

    pub(crate) fn acquire_tref_read_for_transaction_ref_handle(
        &mut self,
        object_table: &mut ObjectTable,
        handle: u32,
    ) -> Result<bool> {
        let Some(object_id) =
            self.known_persistent_object_id_for_transaction_ref_handle(object_table, handle)?
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
            self.known_persistent_object_id_for_transaction_ref_handle(object_table, handle)?
        else {
            return Ok(false);
        };
        self.acquire_object_write(object_table, object_id)
    }

    pub(crate) fn read_object_payload(
        &mut self,
        object_table: &mut ObjectTable,
        object_id: ObjectId,
    ) -> Result<ObjectPayload> {
        if let Some(record) = self.staged_objects.get(&object_id) {
            return Ok(record.payload().clone());
        }
        if self.local_object_table.contains(object_id) {
            return self.local_object_table.payload(object_id);
        }
        object_table.refresh_persistent_object_from_shared_directory(object_id)?;
        let granule = GranuleId::Object { object_id };
        let persistent = object_table
            .live_slot(object_id)
            .ok()
            .map(|slot| slot.persistent);
        if persistent == Some(false) {
            return object_table.payload(object_id);
        }
        if persistent == Some(true) {
            ensure!(
                self.owns_granule_read(granule),
                "transactional object read permission was not acquired"
            );
            let version = self.current_object_version_for_granule(granule, object_table)?;
            self.validate_active_read(granule, version)?;
        }
        self.active_visibility_read_context()?
            .read_object(object_id, || {
                object_table.current_payload_snapshot(object_id)
            })?
            .with_context(|| {
                format!("object is not visible at transaction snapshot: {object_id:?}")
            })
    }

    pub(crate) fn stage_object_payload(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        payload: ObjectPayload,
    ) -> Result<bool> {
        if self.local_object_table.contains(object_id) {
            ensure!(
                self.local_object_table.kind(object_id)? == payload.kind(),
                "object payload kind does not match transaction-local object slot kind"
            );
            return Ok(self
                .staged_objects
                .insert(object_id, StagedObjectRecord::new(payload))
                .is_none());
        }
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
        Ok(self
            .staged_objects
            .insert(object_id, StagedObjectRecord::new(payload))
            .is_none())
    }

    pub(crate) fn read_struct_field(
        &mut self,
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        object_table: &mut ObjectTable,
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
        let mut publications = Vec::new();
        let committed = self.commit_object_payloads_into(object_table, &mut publications)?;
        object_table.install_committed_serialized_persistent_publications(&publications)?;
        Ok(committed)
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

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn staged_object_payloads_for_commit(
        &self,
        object_table: &ObjectTable,
    ) -> Result<Vec<(ObjectId, ObjectPayload)>> {
        self.ensure_active()?;
        self.staged_objects
            .iter()
            .filter_map(
                |(&object, record)| match object_table.is_persistent(object) {
                    Ok(true) => Some(Ok((object, record.payload().clone()))),
                    Ok(false) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect()
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn prepare_mvcc_object_payload_publications(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<(
        Vec<persist::PendingPublication>,
        Vec<(ObjectId, ObjectPayload)>,
    )> {
        self.ensure_active()?;
        let updates = self
            .staged_objects
            .iter()
            .map(|(&object_id, record)| {
                let payload = record.payload().clone();
                ensure!(
                    object_table.kind(object_id)? == payload.kind(),
                    "object payload kind does not match object table slot kind"
                );
                Ok((object_id, payload, object_table.is_persistent(object_id)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let reserved_object_versions = if let Some(runtime) = &self.shared_region_runtime {
            runtime.reserve_persistent_object_record_versions(
                updates
                    .iter()
                    .filter_map(|(object_id, _, persistent)| persistent.then_some(*object_id)),
            )?
        } else {
            BTreeMap::new()
        };
        let mut publications = Vec::new();
        let mut volatile = Vec::new();
        for (object_id, payload, persistent) in updates {
            if !persistent {
                volatile.push((object_id, payload));
                continue;
            }
            let publication =
                if let Some(version) = reserved_object_versions.get(&object_id).copied() {
                    object_table
                        .persistent_object_pending_publication_from_payload_with_record_version(
                            object_id, &payload, version,
                        )?
                } else {
                    object_table
                        .persistent_object_pending_publication_from_payload(object_id, &payload)?
                };
            publications.push(publication);
        }
        Ok((publications, volatile))
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(crate) fn object_is_newly_allocated_for_commit(&self, object: ObjectId) -> Result<bool> {
        self.ensure_active()?;
        Ok(self.allocated_objects.contains(&object))
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
            for to in object_table.trace_persistent_publication_object_ids(publication)? {
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
        object_table: &mut ObjectTable,
        delta: &PersistentGcCommitDelta,
    ) -> Result<PersistentGcStepReport> {
        ensure!(
            self.active.is_none() && current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active"
        );
        let shared_epoch = self.current_shared_persistent_gc_epoch()?;
        if delta.invalidates_reachable_cache {
            self.persistent_gc_state = None;
        }
        self.reset_stale_shared_persistent_gc_state(shared_epoch);
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
            None => self
                .persistent_gc_state
                .insert(Self::new_persistent_gc_state_for_epoch(
                    object_table,
                    committed_roots.unwrap_or_default(),
                    shared_epoch,
                )?),
        };
        if shared_epoch.is_some() {
            state.set_shared_epoch(shared_epoch);
        }
        state.observe_commit_delta(object_table, delta, PersistentGcBudget::objects(1))
    }

    pub(crate) fn observe_persistent_gc_commit_delta_after_commit_best_effort(
        &mut self,
        object_table: &mut ObjectTable,
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
        let result = self.bump_active_versioned_write_granules();
        Self::combine_results(
            result,
            self.clear_active(),
            "failed to clear transaction after durable commit error",
        )
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

    fn ensure_persistent_gc_inactive(&self, message: &'static str) -> Result<()> {
        ensure!(
            self.active.is_none()
                && self.suspended.is_empty()
                && current_thread_transaction().is_none(),
            message
        );
        Ok(())
    }

    #[cfg(feature = "transaction-mvcc")]
    fn with_persistent_gc_policy<T>(
        &mut self,
        objects: &mut ObjectTable,
        operation: impl FnOnce(
            &dyn TransactionPersistentGc,
            Option<&dyn TransactionMvccGcView>,
            &mut Self,
            &mut ObjectTable,
        ) -> Result<T>,
    ) -> Result<T> {
        let shared_runtime = self.shared_region_runtime.clone();
        let visibility = if let Some(runtime) = &shared_runtime {
            runtime.visibility()
        } else {
            self.visibility.clone()
        };

        // Shared admission and policy selection happen together only after the
        // MVCC barrier. Declaration order ensures the shared admission leaves
        // before the MVCC permit on every path.
        let mvcc_permit = visibility.begin_gc_barrier()?;
        let shared_admission = shared_runtime
            .as_ref()
            .map(TransactionRegionRuntime::begin_persistent_gc_with_policy)
            .transpose()?;
        let local_policy = shared_runtime
            .is_none()
            .then(|| self.persistent_gc_policy.clone());
        let policy = match (&shared_admission, &local_policy) {
            (Some(admission), None) => admission.policy(),
            (None, Some(policy)) => policy.as_ref(),
            _ => unreachable!("persistent GC policy authority must be selected exactly once"),
        };
        let rebase = visibility.runtime().prepare_full_rebase(&mvcc_permit)?;
        let validated = rebase.validate_current_objects(objects)?;
        let metadata = visibility
            .runtime()
            .complete_full_rebase(&mvcc_permit, validated)?;
        let mvcc: Option<&dyn TransactionMvccGcView> = match policy.mvcc_mode() {
            GcMvccMode::CurrentStateOnly => None,
            GcMvccMode::MvccCompliant => Some(&metadata),
        };
        operation(policy, mvcc, self, objects)
    }

    #[cfg(not(feature = "transaction-mvcc"))]
    fn with_persistent_gc_policy<T>(
        &mut self,
        objects: &mut ObjectTable,
        operation: impl FnOnce(
            &dyn TransactionPersistentGc,
            Option<&dyn TransactionMvccGcView>,
            &mut Self,
            &mut ObjectTable,
        ) -> Result<T>,
    ) -> Result<T> {
        let shared_runtime = self.shared_region_runtime.clone();
        let shared_admission = shared_runtime
            .as_ref()
            .map(TransactionRegionRuntime::begin_persistent_gc_with_policy)
            .transpose()?;
        let local_policy = shared_runtime
            .is_none()
            .then(|| self.persistent_gc_policy.clone());
        let policy = match (&shared_admission, &local_policy) {
            (Some(admission), None) => admission.policy(),
            (None, Some(policy)) => policy.as_ref(),
            _ => unreachable!("persistent GC policy authority must be selected exactly once"),
        };
        operation(policy, None, self, objects)
    }

    pub(crate) fn persistent_mark_sweep_collect(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport> {
        self.ensure_persistent_gc_inactive(
            "persistent object marker cannot run while a transaction is active or suspended",
        )?;
        self.with_persistent_gc_policy(objects, |policy, mvcc, state, objects| {
            policy.persistent_mark_sweep_collect(mvcc, state, objects)
        })
    }

    pub(super) fn persistent_mark_sweep_collect_uncoordinated(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport> {
        let roots = self.persistent_root_ids()?;
        let report = objects.persistent_mark_sweep_from_roots_uncoordinated(roots)?;
        self.persistent_gc_state = None;
        Ok(report)
    }

    pub(crate) fn persistent_gc_maintenance_step(
        &mut self,
        object_table: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        let result = self
            .ensure_persistent_gc_inactive(
                "persistent object marker cannot run while a transaction is active or suspended",
            )
            .and_then(|()| {
                self.with_persistent_gc_policy(object_table, |policy, mvcc, state, object_table| {
                    policy.persistent_gc_maintenance_step(mvcc, state, object_table, budget)
                })
            });
        if result.is_err() {
            self.persistent_gc_state = None;
        }
        result
    }

    pub(super) fn persistent_gc_maintenance_step_uncoordinated(
        &mut self,
        object_table: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        let shared_epoch = self.current_shared_persistent_gc_epoch()?;
        self.reset_stale_shared_persistent_gc_state(shared_epoch);
        let roots = if self.persistent_gc_state.is_none() {
            Some(self.persistent_root_ids()?)
        } else {
            None
        };
        let state = match self.persistent_gc_state.as_mut() {
            Some(state) => state,
            None => self
                .persistent_gc_state
                .insert(Self::new_persistent_gc_state_for_epoch(
                    object_table,
                    roots.unwrap_or_default(),
                    shared_epoch,
                )?),
        };
        if shared_epoch.is_some() {
            state.set_shared_epoch(shared_epoch);
        }
        state.mark_step(object_table, budget)
    }

    pub(crate) fn finish_persistent_gc_cycle_and_sweep(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>> {
        self.ensure_persistent_gc_inactive(
            "persistent object marker cannot run while a transaction is active or suspended",
        )?;
        self.with_persistent_gc_policy(objects, |policy, mvcc, state, objects| {
            policy.finish_persistent_gc_cycle_and_sweep(mvcc, state, objects)
        })
    }

    pub(super) fn finish_persistent_gc_cycle_and_sweep_uncoordinated(
        &mut self,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>> {
        let shared_epoch = self.current_shared_persistent_gc_epoch()?;
        let rebuild_stale_state = self.reset_stale_shared_persistent_gc_state(shared_epoch);
        let mut state = match self.persistent_gc_state.take() {
            Some(state) => state,
            None if rebuild_stale_state => Self::new_persistent_gc_state_for_epoch(
                objects,
                self.persistent_root_ids()?,
                shared_epoch,
            )?,
            None => {
                return Ok(None);
            }
        };
        if shared_epoch.is_some() {
            state.set_shared_epoch(shared_epoch);
        }
        while !state.is_complete() {
            let pending = state.pending_object_count();
            state.mark_step(objects, PersistentGcBudget::objects(pending.max(1)))?;
        }
        let mark = state.into_report(objects)?;
        mark.ensure_sweepable()?;
        let sweep = objects.apply_volatile_persistent_sweep_uncoordinated(&mark)?;
        Ok(Some(PersistentMarkSweepReport { mark, sweep }))
    }

    fn current_shared_persistent_gc_epoch(&self) -> Result<Option<u64>> {
        self.shared_region_runtime
            .as_ref()
            .map(TransactionRegionRuntime::persistent_gc_epoch)
            .transpose()
    }

    fn reset_stale_shared_persistent_gc_state(&mut self, shared_epoch: Option<u64>) -> bool {
        let Some(shared_epoch) = shared_epoch else {
            return false;
        };
        let stale = self
            .persistent_gc_state
            .as_ref()
            .is_some_and(|state| state.shared_epoch() != Some(shared_epoch));
        if stale {
            self.persistent_gc_state = None;
        }
        stale
    }

    fn new_persistent_gc_state_for_epoch(
        object_table: &mut ObjectTable,
        roots: BTreeSet<ObjectId>,
        shared_epoch: Option<u64>,
    ) -> Result<PersistentGcState> {
        let mut state = PersistentGcState::new(object_table, roots)?;
        if shared_epoch.is_some() {
            state.set_shared_epoch(shared_epoch);
        }
        Ok(state)
    }

    pub(crate) fn compact_persistent_object_chunks(
        &mut self,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport> {
        self.ensure_persistent_gc_inactive(
            "persistent object compaction cannot run while a transaction is active or suspended",
        )?;
        self.with_persistent_gc_policy(objects, |policy, mvcc, state, objects| {
            policy.compact_persistent_object_chunks(mvcc, state, objects, recovery_report)
        })
    }

    pub(super) fn compact_persistent_object_chunks_uncoordinated(
        &mut self,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport> {
        recovery_report.mark.ensure_sweepable()?;

        #[derive(Default)]
        struct ChunkCandidate {
            reachable_objects: BTreeSet<ObjectId>,
            old_locations: Vec<PersistentRecoveredRecordLocation>,
            has_unreachable: bool,
        }

        let mut chunks = BTreeMap::<u32, ChunkCandidate>::new();
        let consumed =
            self.durable_log
                .with_recovered_region_snapshot(&mut |recovered, mapped_source| {
                    let _mapped_source = mapped_source
                        .context("persistent object compaction requires mapped durable region")?;
                    let winners = recovered.committed_object_winners()?;
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
                    Ok(())
                })?;
        ensure!(
            consumed,
            "persistent object compaction requires a durable block region backend"
        );

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
        let reserved_object_versions = if let Some(runtime) = &self.shared_region_runtime {
            runtime.reserve_persistent_object_record_versions(copied_objects.iter().copied())?
        } else {
            BTreeMap::new()
        };
        for object_id in &copied_objects {
            let publication = if let Some(version) =
                reserved_object_versions.get(object_id).copied()
            {
                objects.persistent_gc_copy_publication_with_record_version(*object_id, version)?
            } else {
                objects.persistent_gc_copy_publication(*object_id)?
            };
            publications.push(publication);
        }

        let stream_id =
            u32::try_from(self.next_id).context("GC maintenance transaction id overflow")?;
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("transaction id overflow")?;
        let markers = self.publish_object_publications_before_commit_with_markers(
            stream_id,
            stream_id,
            objects,
            &publications,
        )?;
        let marker = markers
            .last()
            .copied()
            .context("GC maintenance transaction did not publish copied object records")?;
        self.publish_commit_lp(stream_id, stream_id, marker)?;
        self.install_committed_mapped_object_publications(objects, &publications, &markers)?;

        let mut reachable_after = Vec::new();
        let consumed =
            self.durable_log
                .with_recovered_region_snapshot(&mut |recovered, mapped_source| {
                    let _mapped_source = mapped_source
                        .context("persistent object compaction requires mapped durable region")?;
                    let winners_after = recovered.committed_object_winners()?;
                    reachable_after = winners_after
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
                        .collect();
                    Ok(())
                })?;
        ensure!(
            consumed,
            "persistent object compaction requires a durable block region backend"
        );
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
        self.finalize_transaction_local_objects_after_promotion(object_table)?;
        if self.staged_objects.is_empty() {
            return Ok(false);
        }
        let updates = self
            .staged_objects
            .iter()
            .map(|(&object_id, record)| {
                let payload = record.payload();
                ensure!(
                    object_table.kind(object_id)? == payload.kind(),
                    "object payload kind does not match object table slot kind"
                );
                Ok((
                    object_id,
                    payload.clone(),
                    object_table.is_persistent(object_id)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let reserved_object_versions = if let Some(runtime) = &self.shared_region_runtime {
            runtime.reserve_persistent_object_record_versions(
                updates
                    .iter()
                    .filter_map(|(object_id, _, persistent)| persistent.then_some(*object_id)),
            )?
        } else {
            BTreeMap::new()
        };
        for (object_id, payload, persistent) in updates {
            if persistent {
                let publication = if let Some(version) =
                    reserved_object_versions.get(&object_id).copied()
                {
                    object_table
                        .persistent_object_pending_publication_from_payload_with_record_version(
                            object_id, &payload, version,
                        )?
                } else {
                    object_table
                        .persistent_object_pending_publication_from_payload(object_id, &payload)?
                };
                publish(publication)?;
            } else {
                object_table.update_payload(object_id, payload)?;
            }
        }
        self.staged_objects.clear();
        Ok(true)
    }

    pub(crate) fn validate_active_object_reads(&self, object_table: &ObjectTable) -> Result<()> {
        for granule in self.active_read_granules()? {
            if object_granule_object_id(granule).is_none() {
                continue;
            }
            let version = self.current_object_version_for_granule(granule, object_table)?;
            self.validate_active_read(granule, version)?;
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
        #[cfg(feature = "transaction-mvcc")]
        ensure!(
            self.mvcc_terminal_commit.is_none(),
            "MVCC terminal commit retry is pending"
        );
        #[cfg(all(
            test,
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        ))]
        ensure!(
            self.benchmark_mvcc_terminal_commit.is_none(),
            "benchmark MVCC terminal commit retry is pending"
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
            visibility: self.selected_visibility.take(),
            visibility_snapshot: self.visibility_snapshot.take(),
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
            local_object_table: mem::take(&mut self.local_object_table),
            allocated_objects: mem::take(&mut self.allocated_objects),
            conflict_aborted: mem::take(&mut self.active_conflict_aborted),
            read_granules: mem::take(&mut self.read_granules),
            write_granules: mem::take(&mut self.write_granules),
            uncommitted_publication_streams: mem::take(&mut self.uncommitted_publication_streams),
            pending_linear_undo_chunks: mem::take(&mut self.pending_linear_undo_chunks),
            scratch: mem::take(&mut self.scratch),
            pending_memory_store: self.pending_memory_store.take(),
            #[cfg(feature = "transaction-mvcc")]
            mvcc_terminal_commit: self.mvcc_terminal_commit.take(),
            #[cfg(all(
                test,
                feature = "transaction-mvcc",
                feature = "transaction-cc-optimistic-validation"
            ))]
            benchmark_mvcc_terminal_commit: self.benchmark_mvcc_terminal_commit.take(),
        }
    }

    pub(super) fn install_workspace(&mut self, workspace: TransactionWorkspace) {
        self.selected_visibility = workspace.visibility;
        self.visibility_snapshot = workspace.visibility_snapshot;
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
        self.local_object_table = workspace.local_object_table;
        self.allocated_objects = workspace.allocated_objects;
        self.active_conflict_aborted = workspace.conflict_aborted;
        self.read_granules = workspace.read_granules;
        self.write_granules = workspace.write_granules;
        self.uncommitted_publication_streams = workspace.uncommitted_publication_streams;
        self.pending_linear_undo_chunks = workspace.pending_linear_undo_chunks;
        self.scratch = workspace.scratch;
        self.pending_memory_store = workspace.pending_memory_store;
        #[cfg(feature = "transaction-mvcc")]
        {
            self.mvcc_terminal_commit = workspace.mvcc_terminal_commit;
        }
        #[cfg(all(
            test,
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        ))]
        {
            self.benchmark_mvcc_terminal_commit = workspace.benchmark_mvcc_terminal_commit;
        }
    }

    pub(super) fn clear_active(&mut self) -> Result<()> {
        let visibility = self.selected_visibility.take();
        let snapshot = self.visibility_snapshot.take();
        let mut result = Ok(());
        if let Some(transaction) = self.active {
            if self.terminal_commit_active {
                if let Some(runtime) = &self.shared_region_runtime {
                    result = Self::combine_results(
                        result,
                        runtime.end_terminal_commit(transaction),
                        "failed to end terminal commit",
                    );
                }
            }
            result = Self::combine_results(
                result,
                self.release_transaction_authority(transaction),
                "failed to release transaction authority",
            );
            if let Some(runtime) = &self.shared_region_runtime {
                let release_result = if self.uncommitted_publication_streams.is_empty() {
                    runtime.release_current_thread_log_segment_reusable()
                } else {
                    runtime.retire_current_thread_log_segment()
                };
                result = Self::combine_results(
                    result,
                    release_result,
                    "failed to release transaction log segment",
                );
            }
        }
        self.active = None;
        self.terminal_commit_active = false;
        replace_current_thread_transaction(None);
        let finish_snapshot = match (visibility, snapshot) {
            (Some(visibility), Some(snapshot)) => visibility.finish_snapshot(snapshot),
            (None, None) => Ok(()),
            (Some(_), None) => Err(crate::format_err!(
                "active transaction visibility runtime is missing its snapshot"
            )),
            (None, Some(_)) => Err(crate::format_err!(
                "active transaction snapshot is missing its visibility runtime"
            )),
        };
        result = Self::combine_results(
            result,
            finish_snapshot,
            "failed to finish transaction visibility snapshot",
        );
        self.install_workspace(TransactionWorkspace::default());
        #[cfg(test)]
        if result.is_err() {
            self.benchmark_cleanup_failed = true;
        }
        result
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
    pub(super) fn persistent_gc_policy_identity_for_test(&self) -> usize {
        Arc::as_ptr(&self.persistent_gc_policy) as *const () as usize
    }

    pub(super) fn set_persistent_gc_policy_for_test(
        &mut self,
        policy: Arc<dyn TransactionPersistentGc>,
    ) {
        self.persistent_gc_policy = policy;
        self.persistent_gc_state = None;
    }

    pub(super) fn new_for_test(transaction: TransactionId) -> Self {
        replace_current_thread_transaction(Some(transaction));
        let mut state = Self::default();
        state.active = Some(transaction);
        state.next_id = transaction.as_raw().saturating_add(1);
        state.selected_visibility = Some(state.visibility.clone());
        state.visibility_snapshot = Some(state.visibility.begin_snapshot().unwrap());
        state
    }

    #[cfg(feature = "transaction-mvcc")]
    pub(super) fn begin_visibility_gc_barrier_for_test(
        &self,
    ) -> Result<super::mvcc::MvccGcBarrierPermit> {
        self.visibility.begin_gc_barrier_for_test()
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
        object_table: &mut ObjectTable,
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
        self.staged_objects
            .get(&object_id)
            .map(StagedObjectRecord::payload)
    }

    pub(super) fn allocated_object_count_for_test(&self) -> usize {
        self.allocated_objects.len() + self.local_object_table.len()
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

    pub(crate) fn commit_single_tmemory_for_benchmark(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
    ) -> Result<bool> {
        #[cfg(all(
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        ))]
        {
            return self.commit_single_tmemory_for_benchmark_mvcc(tmemory);
        }

        #[cfg(not(all(
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        )))]
        {
            let granules = self
                .active_read_granules()?
                .into_iter()
                .chain(self.active_write_granules()?)
                .collect::<BTreeSet<_>>();
            let versions = {
                let tmemory = tmemory
                    .lock()
                    .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
                granules
                    .into_iter()
                    .map(|granule| {
                        let GranuleId::TMemory {
                            instance: None,
                            memory_index: 0,
                            granule_index,
                        } = granule
                        else {
                            bail!("single-tmemory benchmark state contains a non-benchmark granule")
                        };
                        let granule_index = usize::try_from(granule_index)
                            .context("benchmark tmemory granule index does not fit usize")?;
                        Ok((granule, tmemory.granule_version(granule_index)?))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?
            };
            let current_version = |granule| {
                versions
                    .get(&granule)
                    .copied()
                    .context("benchmark tmemory validation granule was not captured")
            };
            self.validate_active_reads_with(current_version)?;
            self.validate_active_writes_with(&mut |granule| {
                versions
                    .get(&granule)
                    .copied()
                    .context("benchmark tmemory validation granule was not captured")
            })?;
            self.begin_terminal_commit()?;
            let wrote = {
                let mut tmemory = tmemory
                    .lock()
                    .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
                self.commit_tmemory_owned(None, 0, &mut *tmemory)?
            };
            self.complete_commit()?;
            Ok(wrote)
        }
    }

    pub(crate) fn take_benchmark_cleanup_failure_for_test(&mut self) -> bool {
        mem::take(&mut self.benchmark_cleanup_failed)
    }

    fn record_benchmark_cleanup_failure_for_test(&mut self, result: &Result<()>) {
        self.benchmark_cleanup_failed |= result.is_err();
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn take_benchmark_committed_mvcc_versions_after_error(&mut self) -> u64 {
        mem::take(&mut self.benchmark_committed_mvcc_versions_after_error)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn commit_single_tmemory_for_benchmark_mvcc(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
    ) -> Result<bool> {
        if self.benchmark_mvcc_terminal_commit.is_some() {
            return self.drive_benchmark_mvcc_terminal_commit(tmemory);
        }

        let (visibility, snapshot, transaction, reads, writes) =
            self.active_mvcc_commit_context()?;
        let shared_runtime = self.shared_region_runtime_for_publication().cloned();
        let user_region_permit = shared_runtime
            .as_ref()
            .map(TransactionRegionRuntime::begin_user_transaction_region)
            .transpose()?;

        if writes.is_empty() {
            self.begin_terminal_commit()?;
            self.complete_commit()?;
            drop(user_region_permit);
            return Ok(false);
        }

        let certification = match self.acquire_active_mvcc_certification(
            &visibility,
            snapshot,
            transaction,
            &reads,
            &writes,
        ) {
            Ok(certification) => certification,
            Err(error) => {
                let cleanup = if self.active_transaction().is_some() {
                    self.abort()
                } else {
                    Ok(())
                };
                self.record_benchmark_cleanup_failure_for_test(&cleanup);
                return combine_operation_and_cleanup_results(
                    Err(error),
                    cleanup,
                    "failed to abort benchmark transaction after MVCC certification conflict",
                );
            }
        };
        let (commit, pending) = match visibility.begin_pending_commit() {
            Ok(pending) => pending,
            Err(error) => {
                let cleanup = if self.active_transaction().is_some() {
                    self.abort()
                } else {
                    Ok(())
                };
                self.record_benchmark_cleanup_failure_for_test(&cleanup);
                return combine_operation_and_cleanup_results(
                    Err(error),
                    cleanup,
                    "failed to abort benchmark transaction after pending-commit registration",
                );
            }
        };

        let publication_ids = (|| -> Result<(u32, u32)> {
            let transaction_id = self.active_transaction_required_raw()?;
            let stream_id = if let Some(runtime) = self.shared_region_runtime_for_publication() {
                runtime.current_thread_log_segment()?.stream_id()
            } else {
                u32::try_from(transaction_id)
                    .context("transaction id does not fit durable tmemory stream id")?
            };
            let txid = u32::try_from(transaction_id)
                .context("transaction id does not fit durable transaction id")?;
            Ok((stream_id, txid))
        })();
        let (stream_id, txid) = match publication_ids {
            Ok(ids) => ids,
            Err(error) => {
                return self.finish_benchmark_mvcc_uninstalled_failure(
                    tmemory,
                    visibility,
                    commit,
                    pending,
                    certification,
                    user_region_permit,
                    PreparedDomainValues::default(),
                    0,
                    0,
                    error,
                );
            }
        };

        let preparation = (|| -> Result<PreparedDomainValues> {
            let staged = self.staged_tmemory_granules_owned(None, 0)?;
            let tmemory_guard = tmemory
                .lock()
                .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
            let mut physical_values = PreparedDomainValues::default();
            for (granule, granule_index, value) in staged {
                let addr = u64::try_from(granule_index)
                    .context("benchmark tmemory granule index does not fit u64")?
                    .checked_mul(u64::try_from(TMEMORY_GRANULE_SIZE).unwrap())
                    .context("benchmark tmemory granule address overflow")?;
                let predecessor =
                    collect_tmemory_access_snapshot(&tmemory_guard, addr, value.len())?.bytes;
                ensure!(
                    physical_values
                        .memories
                        .insert(granule, PreparedValue { predecessor, value })
                        .is_none(),
                    "benchmark tmemory granule was prepared more than once"
                );
            }
            drop(tmemory_guard);
            visibility.run_predecessor_collected_hook_for_test()?;
            let mut prepare = visibility.begin_prepare(commit.clone())?;
            prepare.append_prepared_values(physical_values)?;
            Ok(prepare.into_values())
        })();
        let prepared = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                return self.finish_benchmark_mvcc_uninstalled_failure(
                    tmemory,
                    visibility,
                    commit,
                    pending,
                    certification,
                    user_region_permit,
                    PreparedDomainValues::default(),
                    stream_id,
                    txid,
                    error,
                );
            }
        };

        if let Err(error) = self.begin_terminal_commit() {
            return self.finish_benchmark_mvcc_uninstalled_failure(
                tmemory,
                visibility,
                commit,
                pending,
                certification,
                user_region_permit,
                prepared,
                stream_id,
                txid,
                error,
            );
        }

        self.benchmark_mvcc_terminal_commit = Some(BenchmarkMvccTerminalCommitState {
            visibility,
            commit,
            pending: Some(pending),
            certification: Some(certification),
            user_region_permit,
            wrote: !prepared.memories.is_empty(),
            prepared,
            installed: BTreeSet::new(),
            undo_published: BTreeSet::new(),
            stream_id,
            txid,
            final_marker: None,
            lp_published: false,
            version_bumps_applied: false,
            decision: MvccTerminalDecision::Abortable,
            completion_error: None,
        });
        self.drive_benchmark_mvcc_terminal_commit(tmemory)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn benchmark_mvcc_version_count_for_commit(&self) -> Result<u64> {
        let count = match &self.benchmark_mvcc_terminal_commit {
            Some(terminal) => terminal.prepared.memories.len(),
            None => self
                .staged_granules
                .keys()
                .filter(|granule| {
                    matches!(
                        granule,
                        GranuleId::TMemory {
                            instance: None,
                            memory_index: 0,
                            ..
                        }
                    )
                })
                .count(),
        };
        u64::try_from(count).context("benchmark MVCC created-version count does not fit u64")
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    pub(crate) fn has_benchmark_mvcc_terminal_for_test(&self) -> bool {
        self.benchmark_mvcc_terminal_commit.is_some()
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn finish_benchmark_mvcc_uninstalled_failure(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        visibility: Arc<mvcc::MvccRuntime>,
        commit: Arc<mvcc::CommitRecord>,
        pending: mvcc::PendingCommitRegistration,
        certification: concurrency::MvccCertificationPermit,
        user_region_permit: Option<region_runtime::UserTransactionRegionPermit>,
        prepared: PreparedDomainValues,
        stream_id: u32,
        txid: u32,
        error: crate::Error,
    ) -> Result<bool> {
        let terminal = BenchmarkMvccTerminalCommitState {
            visibility,
            commit,
            pending: Some(pending),
            certification: Some(certification),
            user_region_permit,
            wrote: !prepared.memories.is_empty(),
            prepared,
            installed: BTreeSet::new(),
            undo_published: BTreeSet::new(),
            stream_id,
            txid,
            final_marker: None,
            lp_published: false,
            version_bumps_applied: false,
            decision: MvccTerminalDecision::Abortable,
            completion_error: None,
        };
        self.finish_benchmark_mvcc_pre_decision_failure(tmemory, terminal, format!("{error:#}"))
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn drive_benchmark_mvcc_terminal_commit(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
    ) -> Result<bool> {
        let mut terminal = self
            .benchmark_mvcc_terminal_commit
            .take()
            .context("benchmark MVCC terminal commit state is missing")?;
        if terminal.decision == MvccTerminalDecision::Rollback {
            let original = terminal
                .completion_error
                .clone()
                .unwrap_or_else(|| "benchmark MVCC rollback retry".to_string());
            return self.finish_benchmark_mvcc_pre_decision_failure(tmemory, terminal, original);
        }

        let attempt = (|| -> Result<()> {
            if terminal.decision == MvccTerminalDecision::Abortable {
                terminal
                    .visibility
                    .inject_commit_fault_for_test(MvccCommitFaultPoint::AfterPreparation)?;
                self.install_benchmark_mvcc_current_values(tmemory, &mut terminal)?;
                if let Some(marker) = terminal.final_marker {
                    self.publish_commit_lp(terminal.stream_id, terminal.txid, marker)?;
                    terminal.lp_published = true;
                    terminal.decision = MvccTerminalDecision::ForceCommit;
                    terminal
                        .visibility
                        .inject_commit_fault_for_test(MvccCommitFaultPoint::AfterDurableLp)?;
                }
            }
            self.force_complete_benchmark_mvcc_commit(&mut terminal)
        })();

        match attempt {
            Ok(()) => Ok(terminal.wrote),
            Err(error) if terminal.decision == MvccTerminalDecision::Abortable => self
                .finish_benchmark_mvcc_pre_decision_failure(
                    tmemory,
                    terminal,
                    format!("{error:#}"),
                ),
            Err(error) => {
                let original = format!("{error:#}");
                terminal.completion_error = Some(original.clone());
                let outcome = if terminal.lp_published {
                    "committed durably"
                } else {
                    "committed irrevocably"
                };
                if self.active_transaction().is_none() {
                    self.record_benchmark_committed_mvcc_versions_after_error(&terminal)?;
                    return Err(crate::format_err!(
                        "benchmark transaction {outcome} but completion/cleanup failed: {original}"
                    ));
                }
                match self.force_complete_benchmark_mvcc_commit(&mut terminal) {
                    Ok(()) => {
                        self.record_benchmark_committed_mvcc_versions_after_error(&terminal)?;
                        Err(crate::format_err!(
                            "benchmark transaction {outcome} but completion/cleanup failed: {original}"
                        ))
                    }
                    Err(completion_error) => {
                        if self.active_transaction().is_some() {
                            self.benchmark_mvcc_terminal_commit = Some(terminal);
                        } else {
                            self.record_benchmark_committed_mvcc_versions_after_error(&terminal)?;
                        }
                        Err(crate::format_err!(
                            "benchmark transaction {outcome} but completion/cleanup failed: {original}; force-completion also failed: {completion_error:#}"
                        ))
                    }
                }
            }
        }
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn record_benchmark_committed_mvcc_versions_after_error(
        &mut self,
        terminal: &BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        ensure!(
            terminal.pending.is_none(),
            "benchmark committed-error MVCC versions are not published"
        );
        let created = u64::try_from(terminal.prepared.memories.len())
            .context("benchmark MVCC created-version count does not fit u64")?;
        self.benchmark_committed_mvcc_versions_after_error = self
            .benchmark_committed_mvcc_versions_after_error
            .checked_add(created)
            .context("benchmark committed-error MVCC created-version overflow")?;
        Ok(())
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn install_benchmark_mvcc_current_values(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        terminal: &mut BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        let region = self.shared_region_runtime_for_publication().cloned();
        if let Some(region) = region {
            return region.with_shared_file_backed_tmemory_commit_read_lock(|| {
                self.install_benchmark_mvcc_current_values_inner(tmemory, terminal)
            });
        }
        self.install_benchmark_mvcc_current_values_inner(tmemory, terminal)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn install_benchmark_mvcc_current_values_inner(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        terminal: &mut BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        let values = terminal
            .prepared
            .memories
            .iter()
            .map(|(&granule, value)| (granule, value.value.clone()))
            .collect::<Vec<_>>();
        let mut tmemory = tmemory
            .lock()
            .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;

        if tmemory.backend() != TMemoryBackend::VMemory {
            for (granule, value) in &values {
                if terminal.undo_published.contains(granule) {
                    continue;
                }
                let GranuleId::TMemory {
                    instance: None,
                    memory_index: 0,
                    granule_index,
                } = *granule
                else {
                    bail!("benchmark MVCC installer received a non-benchmark granule")
                };
                let undo = tmemory.prepare_tmemory_undo_record(None, 0, granule_index, value)?;
                terminal.final_marker = Some(self.publish_tmemory_undo_before_in_place_write(
                    terminal.stream_id,
                    terminal.txid,
                    &undo,
                )?);
                terminal.undo_published.insert(*granule);
            }
        }

        for (granule, value) in values {
            if terminal.installed.contains(&granule) {
                continue;
            }
            let GranuleId::TMemory {
                instance: None,
                memory_index: 0,
                granule_index,
            } = granule
            else {
                bail!("benchmark MVCC installer received a non-benchmark granule")
            };
            tmemory.commit_staged_tmemory_granule(granule_index, &value)?;
            terminal.installed.insert(granule);
            terminal
                .visibility
                .inject_commit_fault_for_test(MvccCommitFaultPoint::AfterMemoryInstall)?;
        }
        terminal.visibility.run_inside_install_hook_for_test()?;
        Ok(())
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn force_complete_benchmark_mvcc_commit(
        &mut self,
        terminal: &mut BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        if let Some(pending) = terminal.pending.as_mut() {
            terminal
                .visibility
                .inject_commit_fault_for_test(MvccCommitFaultPoint::BeforeRecordPublication)?;
            let _timestamp = pending.publish()?;
            terminal.pending = None;
            terminal.decision = MvccTerminalDecision::ForceCommit;
        }
        terminal
            .visibility
            .inject_commit_fault_for_test(MvccCommitFaultPoint::AfterRecordTransition)?;
        if !terminal.version_bumps_applied {
            self.prepare_mvcc_version_commit_completion()?;
            terminal.version_bumps_applied = true;
        }
        self.remove_staged_tmemory_granules_owned(None, 0)?;
        terminal
            .visibility
            .inject_commit_fault_for_test(MvccCommitFaultPoint::DuringCleanup)?;
        self.finish_mvcc_commit_cleanup()?;
        terminal.certification.take();
        terminal.user_region_permit.take();
        Ok(())
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn finish_benchmark_mvcc_pre_decision_failure(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        mut terminal: BenchmarkMvccTerminalCommitState,
        original: String,
    ) -> Result<bool> {
        terminal.decision = MvccTerminalDecision::Rollback;
        terminal.completion_error = Some(original.clone());
        let abort_record = match terminal.pending.as_mut() {
            Some(pending) => pending.abort(),
            None => Ok(()),
        };
        if abort_record.is_ok() {
            terminal.pending = None;
        }
        let rollback = self.rollback_benchmark_mvcc_current_values(tmemory, &mut terminal);
        let remove_versions = if abort_record.is_ok() && rollback.is_ok() {
            terminal
                .visibility
                .remove_aborted_commit_versions(&terminal.commit)
        } else {
            Ok(())
        };
        let retry_required = abort_record.is_err() || rollback.is_err() || remove_versions.is_err();
        let abort = if retry_required {
            Ok(())
        } else if self.active_transaction().is_some() {
            self.abort()
        } else {
            Ok(())
        };

        let cleanup = combine_operation_and_cleanup_results(
            abort_record,
            rollback,
            "failed to restore benchmark MVCC current values",
        );
        let cleanup = combine_operation_and_cleanup_results(
            cleanup,
            remove_versions,
            "failed to remove aborted benchmark MVCC versions",
        );
        let cleanup = combine_operation_and_cleanup_results(
            cleanup,
            abort,
            "failed to abort benchmark MVCC transaction",
        );
        match cleanup {
            Ok(()) => Err(crate::format_err!("{original}")),
            Err(cleanup_error) => {
                self.benchmark_cleanup_failed = true;
                if retry_required && self.active_transaction().is_some() {
                    self.benchmark_mvcc_terminal_commit = Some(terminal);
                    Err(crate::format_err!(
                        "{original}; benchmark MVCC rollback requires an explicit retry: {cleanup_error:#}"
                    ))
                } else {
                    Err(crate::format_err!(
                        "{original}; benchmark MVCC rollback/cleanup also failed: {cleanup_error:#}"
                    ))
                }
            }
        }
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn rollback_benchmark_mvcc_current_values(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        terminal: &mut BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        let region = self.shared_region_runtime_for_publication().cloned();
        if let Some(region) = region {
            return region.with_shared_file_backed_tmemory_commit_read_lock(|| {
                self.rollback_benchmark_mvcc_current_values_inner(tmemory, terminal)
            });
        }
        self.rollback_benchmark_mvcc_current_values_inner(tmemory, terminal)
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    fn rollback_benchmark_mvcc_current_values_inner(
        &mut self,
        tmemory: &std::sync::Mutex<TMemory>,
        terminal: &mut BenchmarkMvccTerminalCommitState,
    ) -> Result<()> {
        let installed = terminal.installed.iter().rev().copied().collect::<Vec<_>>();
        let mut tmemory = tmemory
            .lock()
            .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
        if !installed.is_empty() {
            terminal.visibility.run_benchmark_rollback_hook_for_test()?;
        }
        for granule in installed {
            let predecessor = terminal
                .prepared
                .memories
                .get(&granule)
                .context("installed benchmark MVCC granule has no predecessor")?
                .predecessor
                .clone();
            let GranuleId::TMemory { granule_index, .. } = granule else {
                bail!("benchmark MVCC rollback received a non-memory granule")
            };
            tmemory.commit_staged_tmemory_granule(granule_index, &predecessor)?;
            terminal.installed.remove(&granule);
            terminal
                .visibility
                .inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRollback)?;
        }
        Ok(())
    }

    pub(super) fn begin_terminal_commit_with_cleanup_for_test(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<()> {
        self.begin_terminal_commit_with_object_cleanup(object_table)
    }
}
