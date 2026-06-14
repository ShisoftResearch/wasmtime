use super::{ObjectId, ObjectTable, current_thread_transaction};
use crate::prelude::*;
use alloc::collections::{BTreeSet, VecDeque};
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

#[derive(Clone, Debug, Default)]
pub(crate) struct PersistentGcState {
    reachable: BTreeSet<ObjectId>,
    grey: VecDeque<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}

impl PersistentGcState {
    pub(crate) fn seed_roots<I>(&mut self, objects: &ObjectTable, roots: I)
    where
        I: IntoIterator<Item = ObjectId>,
    {
        for root in roots {
            self.seed_root(objects, root);
        }
    }

    pub(crate) fn mark_step(
        &mut self,
        objects: &ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        let mut report = PersistentGcStepReport::default();
        for _ in 0..budget.objects {
            let Some(object) = self.grey.pop_front() else {
                break;
            };
            report.scanned_objects += 1;
            for child in objects
                .trace_object_ids(object)
                .with_context(|| format!("failed to trace persistent object {object:?}"))?
            {
                match objects.live_slot(child) {
                    Ok(slot) if slot.persistent => {
                        if self.mark_reachable(child) {
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

    fn mark_reachable(&mut self, object: ObjectId) -> bool {
        if self.reachable.insert(object) {
            self.grey.push_back(object);
            true
        } else {
            false
        }
    }

    fn seed_root(&mut self, objects: &ObjectTable, root: ObjectId) {
        match objects.live_slot(root) {
            Ok(slot) if slot.persistent => {
                self.mark_reachable(root);
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

pub(crate) struct PersistentObjectMarker;

impl PersistentObjectMarker {
    pub(crate) fn mark<I>(objects: &ObjectTable, roots: I) -> Result<PersistentObjectMarkReport>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        ensure!(
            current_thread_transaction().is_none(),
            "persistent object marker cannot run while a transaction is active"
        );

        let mut state = PersistentGcState::default();
        state.seed_roots(objects, roots);
        while !state.is_complete() {
            state.mark_step(objects, PersistentGcBudget::objects(state.grey.len()))?;
        }
        state.into_report(objects)
    }
}
