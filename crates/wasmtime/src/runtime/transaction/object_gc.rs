use super::visibility::CommitTimestamp;
use super::{ObjectId, ObjectTable, TransactionState, current_thread_transaction};
use crate::prelude::*;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::fmt::Debug;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GcMvccMode {
    CurrentStateOnly,
    MvccCompliant,
}

/// Metadata made available to an MVCC-aware persistent collector after every
/// historical version has been rebased into the current object state.
///
/// This deliberately exposes no object payloads, version chains, or roots.
pub(super) trait TransactionMvccGcView {
    fn oldest_active_snapshot(&self) -> Option<CommitTimestamp>;
    fn visible_timestamp(&self) -> CommitTimestamp;
    fn baseline_timestamp(&self) -> CommitTimestamp;
    fn is_quiescent(&self) -> bool;
}

pub(super) trait TransactionPersistentGc: Debug + Send + Sync + 'static {
    fn mvcc_mode(&self) -> GcMvccMode;

    fn persistent_mark_sweep_collect(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport>;

    fn persistent_gc_maintenance_step(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport>;

    fn finish_persistent_gc_cycle_and_sweep(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>>;

    fn compact_persistent_object_chunks(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport>;
}

#[derive(Debug, Default)]
pub(super) struct CurrentStatePersistentGc;

impl CurrentStatePersistentGc {
    fn ensure_current_state_view(mvcc: Option<&dyn TransactionMvccGcView>) -> Result<()> {
        ensure!(
            mvcc.is_none(),
            "current-state persistent GC cannot receive an MVCC metadata view"
        );
        Ok(())
    }
}

impl TransactionPersistentGc for CurrentStatePersistentGc {
    fn mvcc_mode(&self) -> GcMvccMode {
        GcMvccMode::CurrentStateOnly
    }

    fn persistent_mark_sweep_collect(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport> {
        Self::ensure_current_state_view(mvcc)?;
        state.persistent_mark_sweep_collect_uncoordinated(objects)
    }

    fn persistent_gc_maintenance_step(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        Self::ensure_current_state_view(mvcc)?;
        state.persistent_gc_maintenance_step_uncoordinated(objects, budget)
    }

    fn finish_persistent_gc_cycle_and_sweep(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>> {
        Self::ensure_current_state_view(mvcc)?;
        state.finish_persistent_gc_cycle_and_sweep_uncoordinated(objects)
    }

    fn compact_persistent_object_chunks(
        &self,
        mvcc: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport> {
        Self::ensure_current_state_view(mvcc)?;
        state.compact_persistent_object_chunks_uncoordinated(objects, recovery_report)
    }
}

#[cfg(feature = "transaction-mvcc")]
impl TransactionMvccGcView for super::mvcc::MvccGcMetadata {
    fn oldest_active_snapshot(&self) -> Option<CommitTimestamp> {
        self.oldest_active_snapshot
    }

    fn visible_timestamp(&self) -> CommitTimestamp {
        self.visible_timestamp
    }

    fn baseline_timestamp(&self) -> CommitTimestamp {
        self.baseline_timestamp
    }

    fn is_quiescent(&self) -> bool {
        self.quiescent
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistentRootErrorKind {
    Missing,
    NotPersistent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistentRootError {
    pub(crate) root: ObjectId,
    pub(crate) kind: PersistentRootErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DanglingObjectRefKind {
    Missing,
    NotPersistent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DanglingObjectRef {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
    pub(crate) kind: DanglingObjectRefKind,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectMarkReport {
    pub(crate) reachable: BTreeSet<ObjectId>,
    pub(crate) unreachable_persistent: BTreeSet<ObjectId>,
    pub(crate) dangling_refs: Vec<DanglingObjectRef>,
    pub(crate) invalid_roots: Vec<PersistentRootError>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentVolatileSweepReport {
    pub(crate) removed_objects: Vec<ObjectId>,
    pub(crate) retained_objects: Vec<ObjectId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentMarkSweepReport {
    pub(crate) mark: PersistentObjectMarkReport,
    pub(crate) sweep: PersistentVolatileSweepReport,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectCompactionReport {
    pub(crate) copied_objects: Vec<ObjectId>,
    pub(crate) retired_chunks: Vec<u32>,
    pub(crate) skipped_chunks: Vec<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PersistentRecoveredRecordLocation {
    pub(crate) object_id: ObjectId,
    pub(crate) version: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_len: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentRecoveryGcReport {
    pub(crate) mark: PersistentObjectMarkReport,
    pub(crate) installed_winners: Vec<u64>,
    pub(crate) skipped_unreachable_winners: Vec<u64>,
    pub(crate) reachable_record_locations: Vec<PersistentRecoveredRecordLocation>,
    pub(crate) unreachable_record_locations: Vec<PersistentRecoveredRecordLocation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentGcBudget {
    objects: usize,
}

impl PersistentGcBudget {
    pub(crate) fn objects(objects: usize) -> Self {
        Self { objects }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcStepReport {
    pub(crate) scanned_objects: usize,
    pub(crate) enqueued_objects: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PersistentObjectEdge {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcCommitDelta {
    pub(crate) new_roots: BTreeSet<ObjectId>,
    pub(crate) edges: BTreeSet<PersistentObjectEdge>,
    pub(crate) invalidates_reachable_cache: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PersistentGcState {
    shared_epoch: Option<u64>,
    reachable: BTreeSet<ObjectId>,
    grey: Vec<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}

impl PersistentObjectMarkReport {
    pub(crate) fn has_integrity_diagnostics(&self) -> bool {
        !self.invalid_roots.is_empty() || !self.dangling_refs.is_empty()
    }

    pub(crate) fn ensure_sweepable(&self) -> Result<()> {
        ensure!(
            !self.has_integrity_diagnostics(),
            "persistent object sweep requires a valid mark graph: {} invalid roots, {} dangling refs",
            self.invalid_roots.len(),
            self.dangling_refs.len()
        );
        Ok(())
    }
}

impl PersistentGcState {
    pub(crate) fn new<I>(objects: &mut ObjectTable, roots: I) -> Result<Self>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        ensure_marker_inactive()?;
        let mut state = Self {
            shared_epoch: None,
            reachable: BTreeSet::new(),
            grey: Vec::new(),
            dangling_refs: Vec::new(),
            invalid_roots: Vec::new(),
        };
        for root in roots {
            state.seed_root(objects, root)?;
        }
        Ok(state)
    }

    pub(crate) fn observe_commit_delta(
        &mut self,
        objects: &mut ObjectTable,
        delta: &PersistentGcCommitDelta,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        ensure_marker_inactive()?;
        for &root in &delta.new_roots {
            self.seed_root(objects, root)?;
        }
        for edge in &delta.edges {
            if self.reachable.contains(&edge.from) {
                self.enqueue_reachable(edge.to);
            }
        }
        self.mark_step(objects, budget)
    }

    pub(crate) fn mark_step(
        &mut self,
        objects: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        ensure_marker_inactive()?;
        ensure!(
            budget.objects != 0 || self.grey.is_empty(),
            "persistent object marker budget must scan at least one object"
        );
        let mut report = PersistentGcStepReport::default();
        for _ in 0..budget.objects {
            let Some(object) = self.grey.pop() else {
                break;
            };
            report.scanned_objects += 1;
            objects.refresh_persistent_object_from_shared_directory(object)?;
            for child in objects
                .trace_object_ids(object)
                .with_context(|| format!("failed to trace persistent object {object:?}"))?
            {
                objects.refresh_persistent_object_from_shared_directory(child)?;
                match objects.live_slot(child) {
                    Ok(slot) if slot.persistent => {
                        if self.enqueue_reachable(child) {
                            report.enqueued_objects += 1;
                        }
                    }
                    Ok(_) => self.dangling_refs.push(DanglingObjectRef {
                        from: object,
                        to: child,
                        kind: DanglingObjectRefKind::NotPersistent,
                    }),
                    Err(_) => self.dangling_refs.push(DanglingObjectRef {
                        from: object,
                        to: child,
                        kind: DanglingObjectRefKind::Missing,
                    }),
                }
            }
        }
        Ok(report)
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.grey.is_empty()
    }

    pub(crate) fn shared_epoch(&self) -> Option<u64> {
        self.shared_epoch
    }

    pub(crate) fn set_shared_epoch(&mut self, shared_epoch: Option<u64>) {
        self.shared_epoch = shared_epoch;
    }

    pub(crate) fn pending_object_count(&self) -> usize {
        self.grey.len()
    }

    pub(crate) fn into_report(self, objects: &ObjectTable) -> Result<PersistentObjectMarkReport> {
        ensure_marker_inactive()?;
        ensure!(
            self.is_complete(),
            "persistent object marker cannot report with pending work"
        );
        let unreachable_persistent = objects
            .persistent_object_ids()?
            .difference(&self.reachable)
            .copied()
            .collect();
        Ok(PersistentObjectMarkReport {
            reachable: self.reachable,
            unreachable_persistent,
            dangling_refs: self.dangling_refs,
            invalid_roots: self.invalid_roots,
        })
    }

    fn enqueue_reachable(&mut self, object: ObjectId) -> bool {
        if self.reachable.insert(object) {
            self.grey.push(object);
            true
        } else {
            false
        }
    }

    fn seed_root(&mut self, objects: &mut ObjectTable, root: ObjectId) -> Result<()> {
        objects.refresh_persistent_object_from_shared_directory(root)?;
        match objects.live_slot(root) {
            Ok(slot) if slot.persistent => {
                self.enqueue_reachable(root);
            }
            Ok(_) => self.invalid_roots.push(PersistentRootError {
                root,
                kind: PersistentRootErrorKind::NotPersistent,
            }),
            Err(_) => self.invalid_roots.push(PersistentRootError {
                root,
                kind: PersistentRootErrorKind::Missing,
            }),
        }
        Ok(())
    }
}

fn ensure_marker_inactive() -> Result<()> {
    ensure!(
        current_thread_transaction().is_none(),
        "persistent object marker cannot run while a transaction is active"
    );
    Ok(())
}

pub(super) struct PersistentObjectMarker;

impl PersistentObjectMarker {
    pub(super) fn mark<I>(objects: &mut ObjectTable, roots: I) -> Result<PersistentObjectMarkReport>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mut state = PersistentGcState::new(objects, roots)?;
        while !state.is_complete() {
            state.mark_step(objects, PersistentGcBudget::objects(state.grey.len()))?;
        }
        state.into_report(objects)
    }
}
