use super::super::visibility::CommitTimestamp;
use super::{CommitRecord, CommitState, MvccCoordinator, VersionChain};
use crate::prelude::*;
use crate::runtime::transaction::{
    GlobalSnapshot, GranuleId, ObjectId, ObjectPayload, TMEMORY_GRANULE_SIZE, TableGranuleSnapshot,
};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedValue<T> {
    pub(crate) predecessor: T,
    pub(crate) value: T,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PreparedObjectValue {
    pub(crate) predecessor: Option<ObjectPayload>,
    pub(crate) value: ObjectPayload,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PreparedDomainValues {
    pub(crate) memories: BTreeMap<GranuleId, PreparedValue<Vec<u8>>>,
    pub(crate) memory_sizes: BTreeMap<GranuleId, PreparedValue<u64>>,
    pub(crate) globals: BTreeMap<GranuleId, PreparedValue<GlobalSnapshot>>,
    pub(crate) tables: BTreeMap<GranuleId, PreparedValue<TableGranuleSnapshot>>,
    pub(crate) table_sizes: BTreeMap<GranuleId, PreparedValue<u64>>,
    pub(crate) objects: BTreeMap<ObjectId, PreparedObjectValue>,
}

#[derive(Debug)]
struct VersionTable<T> {
    chains: BTreeMap<GranuleId, VersionChain<T>>,
    prune_cursor: Option<GranuleId>,
}

impl<T> Default for VersionTable<T> {
    fn default() -> Self {
        Self {
            chains: BTreeMap::new(),
            prune_cursor: None,
        }
    }
}

#[derive(Debug, Default)]
struct ObjectVersionTable {
    chains: BTreeMap<ObjectId, VersionChain<ObjectPayload>>,
    prune_cursor: Option<ObjectId>,
}

#[derive(Debug, Default)]
struct MvccDomainState {
    memories: VersionTable<Vec<u8>>,
    memory_sizes: VersionTable<u64>,
    globals: VersionTable<GlobalSnapshot>,
    tables: VersionTable<TableGranuleSnapshot>,
    table_sizes: VersionTable<u64>,
    objects: ObjectVersionTable,
}

#[derive(Debug, Default)]
struct MvccDomains {
    state: Mutex<MvccDomainState>,
}

/// Runtime-local MVCC visibility state.
#[derive(Debug, Default)]
pub(crate) struct MvccRuntime {
    pub(super) coordinator: MvccCoordinator,
    domains: MvccDomains,
}

impl MvccRuntime {
    pub(crate) fn begin_prepare(&self, commit: Arc<CommitRecord>) -> Result<MvccPrepareGuard<'_>> {
        ensure!(
            matches!(commit.state(), CommitState::Pending),
            "MVCC preparation requires a pending commit record"
        );
        let baseline_timestamp = self.coordinator.baseline_timestamp()?;
        let baseline_commit = Arc::new(CommitRecord::committed_for_baseline(baseline_timestamp)?);
        Ok(MvccPrepareGuard {
            state: self.domains.lock()?,
            commit,
            baseline_commit,
            values: PreparedDomainValues::default(),
        })
    }

    pub(crate) fn read_memory(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: impl FnOnce() -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        ensure_memory_granule(granule)?;
        let state = self.domains.lock()?;
        let value = read_typed(&state.memories, snapshot, granule, current)?;
        validate_memory_payload(&value)?;
        Ok(value)
    }

    pub(crate) fn read_memory_size(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
    ) -> Result<u64> {
        ensure_memory_size_granule(granule)?;
        let state = self.domains.lock()?;
        read_typed(&state.memory_sizes, snapshot, granule, current)
    }

    pub(crate) fn read_global(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: impl FnOnce() -> Result<GlobalSnapshot>,
    ) -> Result<GlobalSnapshot> {
        ensure_global_granule(granule)?;
        let state = self.domains.lock()?;
        read_typed(&state.globals, snapshot, granule, current)
    }

    pub(crate) fn read_table(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: impl FnOnce() -> Result<TableGranuleSnapshot>,
    ) -> Result<TableGranuleSnapshot> {
        ensure_table_granule(granule)?;
        let state = self.domains.lock()?;
        read_typed(&state.tables, snapshot, granule, current)
    }

    pub(crate) fn read_table_size(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
    ) -> Result<u64> {
        ensure_table_size_granule(granule)?;
        let state = self.domains.lock()?;
        read_typed(&state.table_sizes, snapshot, granule, current)
    }

    pub(crate) fn read_object(
        &self,
        snapshot: CommitTimestamp,
        object: ObjectId,
        current: impl FnOnce() -> Result<Option<ObjectPayload>>,
    ) -> Result<Option<ObjectPayload>> {
        let state = self.domains.lock()?;
        match state.objects.chains.get(&object) {
            Some(chain) => Ok(chain.read_visible(snapshot).cloned()),
            None => current(),
        }
    }

    pub(crate) fn latest_committed_timestamp(&self, granule: GranuleId) -> Result<CommitTimestamp> {
        let baseline = self.coordinator.baseline_timestamp()?;
        self.domains.latest_committed_timestamp(granule, baseline)
    }
}

impl MvccDomains {
    fn lock(&self) -> Result<MutexGuard<'_, MvccDomainState>> {
        self.state
            .lock()
            .map_err(|_| crate::format_err!("MVCC domain lock is poisoned"))
    }

    fn latest_committed_timestamp(
        &self,
        granule: GranuleId,
        baseline: CommitTimestamp,
    ) -> Result<CommitTimestamp> {
        let state = self.lock()?;
        let timestamp = match granule {
            GranuleId::TMemory { .. } => newest_timestamp(&state.memories, granule),
            GranuleId::TMemorySize { .. } => newest_timestamp(&state.memory_sizes, granule),
            GranuleId::TGlobal { .. } => newest_timestamp(&state.globals, granule),
            GranuleId::TTable { .. } => newest_timestamp(&state.tables, granule),
            GranuleId::TTableSize { .. } => newest_timestamp(&state.table_sizes, granule),
            GranuleId::Object { object_id } => state
                .objects
                .chains
                .get(&object_id)
                .and_then(VersionChain::newest_committed_timestamp),
        };
        Ok(timestamp.unwrap_or(baseline))
    }
}

fn newest_timestamp<T>(table: &VersionTable<T>, granule: GranuleId) -> Option<CommitTimestamp> {
    table
        .chains
        .get(&granule)
        .and_then(VersionChain::newest_committed_timestamp)
}

fn read_typed<T: Clone>(
    table: &VersionTable<T>,
    snapshot: CommitTimestamp,
    granule: GranuleId,
    current: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match table.chains.get(&granule) {
        Some(chain) => chain
            .read_visible(snapshot)
            .cloned()
            .ok_or_else(|| crate::format_err!("MVCC chain has no visible value")),
        None => current(),
    }
}

/// Holds the domain metadata mutex across baseline capture and pending append.
#[derive(Debug)]
pub(crate) struct MvccPrepareGuard<'a> {
    state: MutexGuard<'a, MvccDomainState>,
    commit: Arc<CommitRecord>,
    baseline_commit: Arc<CommitRecord>,
    values: PreparedDomainValues,
}

impl MvccPrepareGuard<'_> {
    pub(crate) fn values(&self) -> &PreparedDomainValues {
        &self.values
    }

    pub(crate) fn into_values(self) -> PreparedDomainValues {
        self.values
    }

    pub(crate) fn prepare_memory(
        &mut self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<Vec<u8>>,
        value: Vec<u8>,
    ) -> Result<()> {
        ensure_memory_granule(granule)?;
        validate_memory_payload(&value)?;
        ensure!(
            !self.values.memories.contains_key(&granule),
            "memory granule was prepared more than once"
        );
        let predecessor = prepare_typed(
            &mut self.state.memories,
            granule,
            current,
            value.clone(),
            &self.commit,
            &self.baseline_commit,
            validate_memory_payload,
        )?;
        self.values
            .memories
            .insert(granule, PreparedValue { predecessor, value });
        Ok(())
    }

    pub(crate) fn prepare_memory_size(
        &mut self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
        value: u64,
    ) -> Result<()> {
        ensure_memory_size_granule(granule)?;
        ensure!(
            !self.values.memory_sizes.contains_key(&granule),
            "memory size was prepared more than once"
        );
        let predecessor = prepare_typed(
            &mut self.state.memory_sizes,
            granule,
            current,
            value,
            &self.commit,
            &self.baseline_commit,
            |_| Ok(()),
        )?;
        self.values
            .memory_sizes
            .insert(granule, PreparedValue { predecessor, value });
        Ok(())
    }

    pub(crate) fn prepare_global(
        &mut self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<GlobalSnapshot>,
        value: GlobalSnapshot,
    ) -> Result<()> {
        ensure_global_granule(granule)?;
        ensure!(
            !self.values.globals.contains_key(&granule),
            "global was prepared more than once"
        );
        let predecessor = prepare_typed(
            &mut self.state.globals,
            granule,
            current,
            value,
            &self.commit,
            &self.baseline_commit,
            |_| Ok(()),
        )?;
        self.values
            .globals
            .insert(granule, PreparedValue { predecessor, value });
        Ok(())
    }

    pub(crate) fn prepare_table(
        &mut self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<TableGranuleSnapshot>,
        value: TableGranuleSnapshot,
    ) -> Result<()> {
        ensure_table_granule(granule)?;
        ensure!(
            !self.values.tables.contains_key(&granule),
            "table granule was prepared more than once"
        );
        let predecessor = prepare_typed(
            &mut self.state.tables,
            granule,
            current,
            value.clone(),
            &self.commit,
            &self.baseline_commit,
            |_| Ok(()),
        )?;
        self.values
            .tables
            .insert(granule, PreparedValue { predecessor, value });
        Ok(())
    }

    pub(crate) fn prepare_table_size(
        &mut self,
        granule: GranuleId,
        current: impl FnOnce() -> Result<u64>,
        value: u64,
    ) -> Result<()> {
        ensure_table_size_granule(granule)?;
        ensure!(
            !self.values.table_sizes.contains_key(&granule),
            "table size was prepared more than once"
        );
        let predecessor = prepare_typed(
            &mut self.state.table_sizes,
            granule,
            current,
            value,
            &self.commit,
            &self.baseline_commit,
            |_| Ok(()),
        )?;
        self.values
            .table_sizes
            .insert(granule, PreparedValue { predecessor, value });
        Ok(())
    }

    pub(crate) fn prepare_object(
        &mut self,
        object: ObjectId,
        current: impl FnOnce() -> Result<Option<ObjectPayload>>,
        value: ObjectPayload,
    ) -> Result<()> {
        ensure!(
            !self.values.objects.contains_key(&object),
            "object was prepared more than once"
        );
        let predecessor = match self.state.objects.chains.get_mut(&object) {
            Some(chain) => chain.committed_predecessor().cloned(),
            None => {
                let predecessor = current()?;
                let chain = match predecessor.clone() {
                    Some(value) => VersionChain::new_baseline(self.baseline_commit.clone(), value)?,
                    None => VersionChain::new_pending(),
                };
                self.state.objects.chains.insert(object, chain);
                predecessor
            }
        };
        self.state
            .objects
            .chains
            .get_mut(&object)
            .unwrap()
            .push_newest(self.commit.clone(), value.clone());
        self.values
            .objects
            .insert(object, PreparedObjectValue { predecessor, value });
        Ok(())
    }
}

fn prepare_typed<T: Clone>(
    table: &mut VersionTable<T>,
    granule: GranuleId,
    current: impl FnOnce() -> Result<T>,
    value: T,
    commit: &Arc<CommitRecord>,
    baseline_commit: &Arc<CommitRecord>,
    validate: impl FnOnce(&T) -> Result<()>,
) -> Result<T> {
    let predecessor = match table.chains.get_mut(&granule) {
        Some(chain) => chain
            .committed_predecessor()
            .cloned()
            .ok_or_else(|| crate::format_err!("MVCC chain has no committed predecessor"))?,
        None => {
            let predecessor = current()?;
            validate(&predecessor)?;
            table.chains.insert(
                granule,
                VersionChain::new_baseline(baseline_commit.clone(), predecessor.clone())?,
            );
            predecessor
        }
    };
    table
        .chains
        .get_mut(&granule)
        .unwrap()
        .push_newest(commit.clone(), value);
    Ok(predecessor)
}

fn validate_memory_payload(value: &Vec<u8>) -> Result<()> {
    ensure!(
        value.len() <= TMEMORY_GRANULE_SIZE,
        "memory granule payload exceeds {} bytes",
        TMEMORY_GRANULE_SIZE
    );
    Ok(())
}

fn ensure_memory_granule(granule: GranuleId) -> Result<()> {
    ensure!(
        matches!(granule, GranuleId::TMemory { .. }),
        "expected a tmemory granule"
    );
    Ok(())
}

fn ensure_memory_size_granule(granule: GranuleId) -> Result<()> {
    ensure!(
        matches!(granule, GranuleId::TMemorySize { .. }),
        "expected a tmemory-size granule"
    );
    Ok(())
}

fn ensure_global_granule(granule: GranuleId) -> Result<()> {
    ensure!(
        matches!(granule, GranuleId::TGlobal { .. }),
        "expected a tglobal granule"
    );
    Ok(())
}

fn ensure_table_granule(granule: GranuleId) -> Result<()> {
    ensure!(
        matches!(granule, GranuleId::TTable { .. }),
        "expected a ttable granule"
    );
    Ok(())
}

fn ensure_table_size_granule(granule: GranuleId) -> Result<()> {
    ensure!(
        matches!(granule, GranuleId::TTableSize { .. }),
        "expected a ttable-size granule"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::granule::TTABLE_GRANULE_SIZE;
    use crate::runtime::transaction::{
        GranuleId, ObjectId, ObjectPayload, ObjectValue, TMEMORY_GRANULE_SIZE,
        TableElementSnapshot, TableGranuleSnapshot,
    };
    use alloc::sync::Arc;
    use std::sync::mpsc;
    use std::sync::{Barrier, Mutex};
    use std::time::Duration;

    fn memory_granule() -> GranuleId {
        GranuleId::TMemory {
            instance: Some(1),
            memory_index: 2,
            granule_index: 3,
        }
    }

    fn memory_size_granule() -> GranuleId {
        GranuleId::TMemorySize {
            instance: Some(1),
            memory_index: 2,
        }
    }

    fn global_granule() -> GranuleId {
        GranuleId::TGlobal {
            instance: Some(1),
            global_index: 4,
        }
    }

    fn table_granule() -> GranuleId {
        GranuleId::TTable {
            instance: Some(1),
            table_index: 5,
            granule_index: 6,
        }
    }

    fn table_size_granule() -> GranuleId {
        GranuleId::TTableSize {
            instance: Some(1),
            table_index: 5,
        }
    }

    fn object_id() -> ObjectId {
        ObjectId { object_index: 7 }
    }

    fn object_payload(value: i32) -> ObjectPayload {
        ObjectPayload::Struct(vec![ObjectValue::I32(value)])
    }

    #[test]
    fn prepared_values_keep_six_domains_typed_and_share_one_commit_record() {
        let runtime = MvccRuntime::default();
        let commit = Arc::new(CommitRecord::pending());
        let table_before =
            TableGranuleSnapshot::new(vec![TableElementSnapshot::FuncRef(10)]).unwrap();
        let table_after =
            TableGranuleSnapshot::new(vec![TableElementSnapshot::FuncRef(20)]).unwrap();
        let mut prepare = runtime.begin_prepare(commit.clone()).unwrap();

        prepare
            .prepare_memory(memory_granule(), || Ok(vec![1]), vec![2])
            .unwrap();
        prepare
            .prepare_memory_size(memory_size_granule(), || Ok(3), 4)
            .unwrap();
        prepare
            .prepare_global(
                global_granule(),
                || Ok(GlobalSnapshot::I32(5)),
                GlobalSnapshot::I64(6),
            )
            .unwrap();
        prepare
            .prepare_table(
                table_granule(),
                || Ok(table_before.clone()),
                table_after.clone(),
            )
            .unwrap();
        prepare
            .prepare_table_size(table_size_granule(), || Ok(7), 8)
            .unwrap();
        prepare
            .prepare_object(
                object_id(),
                || Ok(Some(object_payload(9))),
                object_payload(10),
            )
            .unwrap();

        let values = prepare.values();
        assert_eq!(
            values.memories.get(&memory_granule()).unwrap(),
            &PreparedValue {
                predecessor: vec![1],
                value: vec![2],
            }
        );
        assert_eq!(
            values.memory_sizes.get(&memory_size_granule()).unwrap(),
            &PreparedValue {
                predecessor: 3,
                value: 4,
            }
        );
        assert_eq!(
            values.globals.get(&global_granule()).unwrap(),
            &PreparedValue {
                predecessor: GlobalSnapshot::I32(5),
                value: GlobalSnapshot::I64(6),
            }
        );
        assert_eq!(
            values.tables.get(&table_granule()).unwrap(),
            &PreparedValue {
                predecessor: table_before,
                value: table_after,
            }
        );
        assert_eq!(
            values.table_sizes.get(&table_size_granule()).unwrap(),
            &PreparedValue {
                predecessor: 7,
                value: 8,
            }
        );
        assert_eq!(
            values.objects.get(&object_id()).unwrap(),
            &PreparedObjectValue {
                predecessor: Some(object_payload(9)),
                value: object_payload(10),
            }
        );

        let state = &prepare.state;
        assert!(Arc::ptr_eq(
            state
                .memories
                .chains
                .get(&memory_granule())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
        assert!(Arc::ptr_eq(
            state
                .memory_sizes
                .chains
                .get(&memory_size_granule())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
        assert!(Arc::ptr_eq(
            state
                .globals
                .chains
                .get(&global_granule())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
        assert!(Arc::ptr_eq(
            state
                .tables
                .chains
                .get(&table_granule())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
        assert!(Arc::ptr_eq(
            state
                .table_sizes
                .chains
                .get(&table_size_granule())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
        assert!(Arc::ptr_eq(
            state
                .objects
                .chains
                .get(&object_id())
                .unwrap()
                .newest_commit_record()
                .unwrap(),
            &commit
        ));
    }

    #[test]
    fn baseline_fallback_and_prepare_are_one_mutex_operation() {
        let runtime = Arc::new(MvccRuntime::default());
        let entered_fallback = Arc::new(Barrier::new(2));
        let release_fallback = Arc::new(Barrier::new(2));

        let reader_runtime = runtime.clone();
        let reader_lock_probe = runtime.clone();
        let reader_entered = entered_fallback.clone();
        let reader_release = release_fallback.clone();
        let reader = std::thread::spawn(move || {
            reader_runtime.read_memory(0, memory_granule(), || {
                assert!(reader_lock_probe.domains.state.try_lock().is_err());
                reader_entered.wait();
                reader_release.wait();
                Ok(vec![7])
            })
        });
        entered_fallback.wait();

        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let prepare_runtime = runtime.clone();
        let prepare = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let commit = Arc::new(CommitRecord::pending());
            let mut prepare = prepare_runtime.begin_prepare(commit).unwrap();
            prepare
                .prepare_memory(memory_granule(), || Ok(vec![7]), vec![8])
                .unwrap();
            finished_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        release_fallback.wait();

        assert_eq!(reader.join().unwrap().unwrap(), vec![7]);
        finished_rx.recv().unwrap();
        prepare.join().unwrap();

        let state = runtime.domains.state.lock().unwrap();
        let chain = state.memories.chains.get(&memory_granule()).unwrap();
        assert_eq!(chain.version_count(), 2);
        assert_eq!(chain.read_visible(0), Some(&vec![7]));
        assert_eq!(
            chain.newest_commit_record().unwrap().state(),
            CommitState::Pending
        );
    }

    #[test]
    fn latest_timestamp_for_unversioned_granules_is_runtime_baseline() {
        let runtime = MvccRuntime::default();
        let granules = [
            memory_granule(),
            memory_size_granule(),
            global_granule(),
            table_granule(),
            table_size_granule(),
            GranuleId::Object {
                object_id: object_id(),
            },
        ];

        for granule in granules {
            assert_eq!(
                runtime.latest_committed_timestamp(granule).unwrap(),
                runtime.coordinator.baseline_timestamp().unwrap()
            );
        }
    }

    #[test]
    fn new_object_is_invisible_before_its_creation_timestamp() {
        let runtime = MvccRuntime::default();
        let commit = Arc::new(CommitRecord::pending());
        let mut prepare = runtime.begin_prepare(commit.clone()).unwrap();
        prepare
            .prepare_object(object_id(), || Ok(None), object_payload(11))
            .unwrap();
        drop(prepare);

        assert_eq!(
            runtime
                .read_object(0, object_id(), || panic!("chain already exists"))
                .unwrap(),
            None
        );
        commit.commit(1).unwrap();
        assert_eq!(
            runtime
                .read_object(0, object_id(), || panic!("chain already exists"))
                .unwrap(),
            None
        );
        assert_eq!(
            runtime
                .read_object(1, object_id(), || panic!("chain already exists"))
                .unwrap(),
            Some(object_payload(11))
        );
    }

    #[test]
    fn table_granule_snapshot_validates_element_limit_and_exposes_elements() {
        let valid =
            vec![TableElementSnapshot::FuncRef(0); usize::try_from(TTABLE_GRANULE_SIZE).unwrap()];
        let snapshot = TableGranuleSnapshot::new(valid.clone()).unwrap();
        assert_eq!(snapshot.elements(), valid.as_slice());

        let oversized = vec![
            TableElementSnapshot::FuncRef(0);
            usize::try_from(TTABLE_GRANULE_SIZE).unwrap() + 1
        ];
        assert_eq!(
            TableGranuleSnapshot::new(oversized)
                .unwrap_err()
                .to_string(),
            "table granule snapshot exceeds 16 elements"
        );
    }

    #[test]
    fn memory_preparation_rejects_payloads_larger_than_one_granule() {
        let runtime = MvccRuntime::default();
        let commit = Arc::new(CommitRecord::pending());
        let mut prepare = runtime.begin_prepare(commit).unwrap();
        let current_called = Arc::new(Mutex::new(false));
        let current_called_in_closure = current_called.clone();

        let error = prepare
            .prepare_memory(
                memory_granule(),
                move || {
                    *current_called_in_closure.lock().unwrap() = true;
                    Ok(vec![0])
                },
                vec![0; TMEMORY_GRANULE_SIZE + 1],
            )
            .unwrap_err();

        assert_eq!(error.to_string(), "memory granule payload exceeds 64 bytes");
        assert!(!*current_called.lock().unwrap());
        assert!(
            !prepare
                .state
                .memories
                .chains
                .contains_key(&memory_granule())
        );
    }

    #[test]
    fn memory_fallback_read_rejects_payloads_larger_than_one_granule() {
        let runtime = MvccRuntime::default();

        let error = runtime
            .read_memory(0, memory_granule(), || {
                Ok(vec![0; TMEMORY_GRANULE_SIZE + 1])
            })
            .unwrap_err();

        assert_eq!(error.to_string(), "memory granule payload exceeds 64 bytes");
    }

    #[test]
    fn prepare_guard_returns_typed_batch_and_releases_domain_mutex() {
        let runtime = MvccRuntime::default();
        let commit = Arc::new(CommitRecord::pending());
        let mut prepare = runtime.begin_prepare(commit).unwrap();
        prepare
            .prepare_memory(memory_granule(), || Ok(vec![1]), vec![2])
            .unwrap();

        let values = prepare.into_values();

        assert_eq!(
            values.memories.get(&memory_granule()).unwrap(),
            &PreparedValue {
                predecessor: vec![1],
                value: vec![2],
            }
        );
        assert_eq!(
            runtime
                .read_memory(0, memory_granule(), || panic!("chain already exists"))
                .unwrap(),
            vec![1]
        );
    }
}
