use super::super::visibility::CommitTimestamp;
use super::GranuleId;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

#[derive(Clone, Debug)]
pub(crate) struct ModelTransaction {
    snapshot: CommitTimestamp,
    reads: BTreeSet<GranuleId>,
    writes: BTreeMap<GranuleId, i64>,
}

impl ModelTransaction {
    pub(crate) fn snapshot(&self) -> CommitTimestamp {
        self.snapshot
    }

    pub(crate) fn reads(&self) -> &BTreeSet<GranuleId> {
        &self.reads
    }

    pub(crate) fn writes(&self) -> &BTreeMap<GranuleId, i64> {
        &self.writes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModelCommitOutcome {
    Committed(Option<CommitTimestamp>),
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelOperation {
    Begin {
        transaction: usize,
    },
    Read {
        transaction: usize,
        granule: GranuleId,
    },
    Write {
        transaction: usize,
        granule: GranuleId,
        value: i64,
    },
    Commit {
        transaction: usize,
    },
}

#[derive(Debug, Default)]
pub(crate) struct SerializableMvccModel {
    visible: CommitTimestamp,
    versions: BTreeMap<GranuleId, Vec<(CommitTimestamp, i64)>>,
}

impl SerializableMvccModel {
    pub(crate) fn with_values(values: impl IntoIterator<Item = (GranuleId, i64)>) -> Self {
        Self {
            visible: 0,
            versions: values
                .into_iter()
                .map(|(granule, value)| (granule, vec![(0, value)]))
                .collect(),
        }
    }

    pub(crate) fn begin(&self) -> ModelTransaction {
        ModelTransaction {
            snapshot: self.visible,
            reads: BTreeSet::new(),
            writes: BTreeMap::new(),
        }
    }

    pub(crate) fn read(&self, transaction: &mut ModelTransaction, granule: GranuleId) -> i64 {
        transaction.reads.insert(granule);
        transaction
            .writes
            .get(&granule)
            .copied()
            .unwrap_or_else(|| self.value_at(transaction.snapshot, granule))
    }

    pub(crate) fn write(&self, transaction: &mut ModelTransaction, granule: GranuleId, value: i64) {
        transaction.writes.insert(granule, value);
    }

    pub(crate) fn commit(&mut self, transaction: ModelTransaction) -> ModelCommitOutcome {
        if transaction.writes.is_empty() {
            return ModelCommitOutcome::Committed(None);
        }
        if transaction
            .reads
            .iter()
            .chain(transaction.writes.keys())
            .copied()
            .any(|granule| self.newest_timestamp(granule) > transaction.snapshot)
        {
            return ModelCommitOutcome::Conflict;
        }
        self.visible = self
            .visible
            .checked_add(1)
            .expect("bounded model timestamp overflow");
        for (granule, value) in transaction.writes {
            self.versions
                .entry(granule)
                .or_default()
                .push((self.visible, value));
        }
        ModelCommitOutcome::Committed(Some(self.visible))
    }

    pub(crate) fn visible_timestamp(&self) -> CommitTimestamp {
        self.visible
    }

    pub(crate) fn visible_value(&self, granule: GranuleId) -> i64 {
        self.value_at(self.visible, granule)
    }

    fn newest_timestamp(&self, granule: GranuleId) -> CommitTimestamp {
        self.versions
            .get(&granule)
            .and_then(|versions| versions.last())
            .map(|(timestamp, _)| *timestamp)
            .unwrap_or(0)
    }

    fn value_at(&self, snapshot: CommitTimestamp, granule: GranuleId) -> i64 {
        self.versions
            .get(&granule)
            .and_then(|versions| {
                versions
                    .iter()
                    .rev()
                    .find(|(timestamp, _)| *timestamp <= snapshot)
            })
            .map(|(_, value)| *value)
            .unwrap_or(0)
    }
}

#[derive(Debug)]
struct FixedRng(u64);

impl FixedRng {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(len).unwrap()).unwrap()
    }
}

fn model_granule(index: usize) -> GranuleId {
    GranuleId::TMemory {
        instance: None,
        memory_index: 0,
        granule_index: u64::try_from(index).unwrap(),
    }
}

pub(crate) fn generate_model_schedules(seed: u64, count: usize) -> Vec<Vec<ModelOperation>> {
    let mut rng = FixedRng(seed);
    let mut schedules = Vec::with_capacity(count);
    for case in 0..count {
        let transaction_count = 2 + rng.index(3);
        let granule_count = 2 + rng.index(3);
        let mut queues = vec![VecDeque::new(); transaction_count];
        for transaction in 0..transaction_count {
            queues[transaction].push_back(ModelOperation::Begin { transaction });
        }

        for granule in 0..granule_count {
            let transaction = granule % transaction_count;
            queues[transaction].push_back(ModelOperation::Read {
                transaction,
                granule: model_granule(granule),
            });
        }
        for (transaction, queue) in queues.iter_mut().enumerate() {
            let read_granule = model_granule(rng.index(granule_count));
            queue.push_back(ModelOperation::Read {
                transaction,
                granule: read_granule,
            });
            let read_only = (case + transaction) % 4 == 0;
            if !read_only {
                queue.push_back(ModelOperation::Write {
                    transaction,
                    granule: model_granule(rng.index(granule_count)),
                    value: i64::try_from(case * 16 + transaction + 1).unwrap(),
                });
                if rng.index(2) == 0 {
                    queue.push_back(ModelOperation::Write {
                        transaction,
                        granule: model_granule(rng.index(granule_count)),
                        value: i64::try_from(case * 32 + transaction + 101).unwrap(),
                    });
                }
            }
            queue.push_back(ModelOperation::Commit { transaction });
        }
        let mut operations = Vec::new();
        while queues.iter().any(|queue| !queue.is_empty()) {
            let eligible = queues
                .iter()
                .enumerate()
                .filter_map(|(transaction, queue)| (!queue.is_empty()).then_some(transaction))
                .collect::<Vec<_>>();
            let transaction = eligible[rng.index(eligible.len())];
            operations.push(queues[transaction].pop_front().unwrap());
        }
        schedules.push(operations);
    }
    schedules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::GranuleId;

    fn granule(index: u64) -> GranuleId {
        GranuleId::TMemory {
            instance: None,
            memory_index: 0,
            granule_index: index,
        }
    }

    #[test]
    fn serializable_model_rejects_a_stale_read_dependency() {
        let mut model = SerializableMvccModel::with_values([(granule(0), 1), (granule(1), 1)]);
        let mut dependent = model.begin();
        assert_eq!(model.read(&mut dependent, granule(0)), 1);
        model.write(&mut dependent, granule(1), 9);
        let mut writer = model.begin();
        model.write(&mut writer, granule(0), 2);

        assert_eq!(model.commit(writer), ModelCommitOutcome::Committed(Some(1)));
        assert_eq!(model.commit(dependent), ModelCommitOutcome::Conflict);
        assert_eq!(model.visible_value(granule(1)), 1);
    }
}
