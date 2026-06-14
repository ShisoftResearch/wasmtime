use super::{ObjectId, ObjectTable, current_thread_transaction};
use crate::prelude::*;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

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
}

#[derive(Clone, Debug)]
pub(crate) struct PersistentGcState {
    reachable: BTreeSet<ObjectId>,
    grey: Vec<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}

impl PersistentGcState {
    pub(crate) fn new<I>(objects: &ObjectTable, roots: I) -> Result<Self>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        ensure_marker_inactive()?;
        let mut state = Self {
            reachable: BTreeSet::new(),
            grey: Vec::new(),
            dangling_refs: Vec::new(),
            invalid_roots: Vec::new(),
        };
        for root in roots {
            state.seed_root(objects, root);
        }
        Ok(state)
    }

    pub(crate) fn observe_commit_delta(
        &mut self,
        objects: &ObjectTable,
        delta: &PersistentGcCommitDelta,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        ensure_marker_inactive()?;
        for &root in &delta.new_roots {
            self.enqueue_reachable(root);
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
        objects: &ObjectTable,
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
            for child in objects
                .trace_object_ids(object)
                .with_context(|| format!("failed to trace persistent object {object:?}"))?
            {
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

    fn seed_root(&mut self, objects: &ObjectTable, root: ObjectId) {
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
    }
}

fn ensure_marker_inactive() -> Result<()> {
    ensure!(
        current_thread_transaction().is_none(),
        "persistent object marker cannot run while a transaction is active"
    );
    Ok(())
}

pub(crate) struct PersistentObjectMarker;

impl PersistentObjectMarker {
    pub(crate) fn mark<I>(objects: &ObjectTable, roots: I) -> Result<PersistentObjectMarkReport>
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
