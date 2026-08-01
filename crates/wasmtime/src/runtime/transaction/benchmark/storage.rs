use super::config::BackendKind;
use crate::prelude::*;
use crate::runtime::transaction::{
    GcMvccMode, ObjectTable, PersistentGcBudget, TransactionRegionRuntime, TransactionState,
    collect_tmemory_access_snapshot, combine_operation_and_cleanup_results,
};
use crate::runtime::vm::TMemory;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CONFLICT_MARKERS: &[&str] = &[
    "transaction read conflict",
    "transaction write conflict",
    "transaction conflict would wait",
    "transaction was conflict-aborted",
    "transaction MVCC certification conflict",
];

pub(super) struct BenchmarkStorage {
    backend: BackendKind,
    runtime: TransactionRegionRuntime,
    tmemory: Mutex<TMemory>,
    file_paths: Option<BenchmarkFilePaths>,
    object_table: Mutex<ObjectTable>,
    mvcc_versions_created: AtomicU64,
}

struct BenchmarkFilePaths {
    directory: tempfile::TempDir,
    tmemory: PathBuf,
    transaction_log: PathBuf,
}

#[derive(Debug)]
pub(super) struct CommitObservation {
    pub wrote: bool,
    pub mvcc_versions_created: u64,
}

pub(super) enum AttemptOutcome<T> {
    Committed(T),
    Conflict,
}

pub(super) struct GcBarrierObservation {
    pub elapsed: Duration,
    pub policy_mode: &'static str,
    pub resident_versions_before: u64,
    pub resident_versions_after: u64,
    pub barrier_removed_versions: u64,
    pub opportunistically_pruned_versions: u64,
}

impl BenchmarkStorage {
    pub(super) fn new(backend: BackendKind) -> Result<Arc<Self>> {
        let (runtime, tmemory, file_paths) = match backend {
            BackendKind::Vmemory => (
                TransactionRegionRuntime::new_for_test(),
                TMemory::new_vmemory(1)?,
                None,
            ),
            BackendKind::FileBacked => {
                let directory = tempfile::Builder::new()
                    .prefix("wasmtime-transaction-benchmark-")
                    .tempdir()?;
                let tmemory_path = directory.path().join("tmemory.bin");
                let transaction_log = directory.path().join("transaction-log.bin");
                let runtime = TransactionRegionRuntime::create_file_backed_for_test(
                    &tmemory_path,
                    &transaction_log,
                    4096,
                )?;
                let shared = runtime
                    .shared_file_backed_storage()?
                    .context("benchmark file-backed runtime has no shared storage config")?;
                let tmemory = TMemory::new(shared.transaction_config()?, 1, Some(1))?;
                runtime.record_file_backed_tmemory_pages(1)?;
                (
                    runtime,
                    tmemory,
                    Some(BenchmarkFilePaths {
                        directory,
                        tmemory: tmemory_path,
                        transaction_log,
                    }),
                )
            }
        };
        let mut object_table = ObjectTable::default();
        object_table.set_shared_region_runtime(Some(runtime.clone()));
        Ok(Arc::new(Self {
            backend,
            runtime,
            tmemory: Mutex::new(tmemory),
            file_paths,
            object_table: Mutex::new(object_table),
            mvcc_versions_created: AtomicU64::new(0),
        }))
    }

    pub(super) fn new_transaction_state(&self) -> Result<TransactionState> {
        let mut state = TransactionState::default();
        if self.backend == BackendKind::FileBacked {
            let shared = self
                .runtime
                .shared_file_backed_storage()?
                .context("benchmark file-backed runtime has no shared storage config")?;
            state.open_shared_file_backed_durable_log(&shared)?;
            state.set_shared_region_runtime(Some(self.runtime.clone()));
        }
        Ok(state)
    }

    pub(super) fn begin(&self, state: &mut TransactionState) -> Result<()> {
        state.begin_with_region_runtime(&self.runtime).map(|_| ())
    }

    pub(super) fn read_u64(&self, state: &mut TransactionState, addr: u64) -> Result<u64> {
        let snapshot = {
            let tmemory = self
                .tmemory
                .lock()
                .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
            collect_tmemory_access_snapshot(&tmemory, addr, size_of::<u64>())?
        };
        let bytes =
            state.read_tmemory_owned_from_snapshot(None, 0, addr, size_of::<u64>(), &snapshot)?;
        Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| {
            crate::format_err!("benchmark u64 read returned the wrong byte count")
        })?))
    }

    pub(super) fn stage_u64(
        &self,
        state: &mut TransactionState,
        addr: u64,
        value: u64,
    ) -> Result<()> {
        let snapshot = {
            let tmemory = self
                .tmemory
                .lock()
                .map_err(|_| crate::format_err!("benchmark tmemory lock poisoned"))?;
            collect_tmemory_access_snapshot(&tmemory, addr, size_of::<u64>())?
        };
        state.stage_tmemory_write_owned_from_snapshot(
            None,
            0,
            addr,
            &value.to_le_bytes(),
            &snapshot,
        )
    }

    pub(super) fn commit(&self, state: &mut TransactionState) -> Result<CommitObservation> {
        #[cfg(all(
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        ))]
        let staged_versions = state.benchmark_mvcc_version_count_for_commit()?;
        #[cfg(not(all(
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        )))]
        let staged_versions = 0;

        let commit = state.commit_single_tmemory_for_benchmark(&self.tmemory);
        #[cfg(all(
            feature = "transaction-mvcc",
            feature = "transaction-cc-optimistic-validation"
        ))]
        self.record_mvcc_versions_created(
            state.take_benchmark_committed_mvcc_versions_after_error(),
        )?;
        let wrote = commit?;
        let mvcc_versions_created = if wrote { staged_versions } else { 0 };
        self.record_mvcc_versions_created(mvcc_versions_created)?;
        Ok(CommitObservation {
            wrote,
            mvcc_versions_created,
        })
    }

    fn record_mvcc_versions_created(&self, count: u64) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        self.mvcc_versions_created
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(count)
            })
            .map_err(|_| crate::format_err!("benchmark MVCC created-version overflow"))?;
        Ok(())
    }

    pub(super) fn finish_attempt<T>(
        &self,
        state: &mut TransactionState,
        result: Result<T>,
    ) -> Result<AttemptOutcome<T>> {
        let result = result.and_then(|value| self.commit(state).map(|_| value));
        let internal_cleanup_failed = state.take_benchmark_cleanup_failure_for_test();
        match result {
            Ok(value) => {
                ensure!(
                    !internal_cleanup_failed,
                    "benchmark transaction cleanup failed before attempt completion"
                );
                Ok(AttemptOutcome::Committed(value))
            }
            Err(error) => {
                let is_conflict = {
                    let message = format!("{error:#}");
                    CONFLICT_MARKERS
                        .iter()
                        .any(|marker| message.contains(marker))
                };
                let cleanup = self.abort_if_active(state);
                let outer_cleanup_failed = state.take_benchmark_cleanup_failure_for_test();
                let cleanup_failed =
                    internal_cleanup_failed || cleanup.is_err() || outer_cleanup_failed;
                if is_conflict && !cleanup_failed {
                    return Ok(AttemptOutcome::Conflict);
                }
                combine_operation_and_cleanup_results(
                    Err(error),
                    cleanup,
                    "failed to abort benchmark transaction attempt",
                )
            }
        }
    }

    pub(super) fn abort_if_active(&self, state: &mut TransactionState) -> Result<()> {
        if state.active_transaction().is_some() {
            state.abort()?;
        }
        Ok(())
    }

    pub(super) fn run_gc_barrier(&self) -> Result<GcBarrierObservation> {
        let mode = self.runtime.persistent_gc_mode_for_test()?;
        let policy_mode = match mode {
            GcMvccMode::CurrentStateOnly => "current-state-only",
            GcMvccMode::MvccCompliant => "mvcc-compliant",
        };
        #[cfg(feature = "transaction-mvcc")]
        let (resident_versions_before, opportunistic_before) = {
            let visibility = self.runtime.visibility_for_test();
            (
                u64::try_from(visibility.runtime().total_version_count_for_test()?)
                    .context("benchmark resident MVCC version count does not fit u64")?,
                visibility.opportunistically_pruned_version_count_for_test(),
            )
        };
        #[cfg(not(feature = "transaction-mvcc"))]
        let (resident_versions_before, opportunistic_before) = (0u64, 0u64);

        let mut state = TransactionState::default();
        state.set_shared_region_runtime(Some(self.runtime.clone()));
        let started = Instant::now();
        state.persistent_gc_maintenance_step(
            &mut *self.object_table.lock().unwrap(),
            PersistentGcBudget::objects(1),
        )?;
        let elapsed = started.elapsed();

        #[cfg(feature = "transaction-mvcc")]
        let (resident_versions_after, opportunistic_after) = {
            let visibility = self.runtime.visibility_for_test();
            (
                u64::try_from(visibility.runtime().total_version_count_for_test()?)
                    .context("benchmark resident MVCC version count does not fit u64")?,
                visibility.opportunistically_pruned_version_count_for_test(),
            )
        };
        #[cfg(not(feature = "transaction-mvcc"))]
        let (resident_versions_after, opportunistic_after) = (0u64, 0u64);

        Ok(GcBarrierObservation {
            elapsed,
            policy_mode,
            resident_versions_before,
            resident_versions_after,
            barrier_removed_versions: resident_versions_before
                .checked_sub(resident_versions_after)
                .context("benchmark GC barrier increased resident MVCC versions")?,
            opportunistically_pruned_versions: opportunistic_after
                .checked_sub(opportunistic_before)
                .context("benchmark opportunistic MVCC pruning counter regressed")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::clear_current_thread_transaction_on_drop_for_test;
    use std::sync::{Arc, Barrier};

    fn committed_u64(storage: &BenchmarkStorage, addr: u64) -> u64 {
        let mut state = storage.new_transaction_state().unwrap();
        storage.begin(&mut state).unwrap();
        let value = storage.read_u64(&mut state, addr).unwrap();
        assert!(matches!(
            storage.finish_attempt(&mut state, Ok(())).unwrap(),
            AttemptOutcome::Committed(())
        ));
        value
    }

    #[test]
    fn vmemory_rmw_commits() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let mut state = storage.new_transaction_state().unwrap();

        storage.begin(&mut state).unwrap();
        let value = storage.read_u64(&mut state, 0).unwrap();
        storage.stage_u64(&mut state, 0, value + 1).unwrap();
        let observation = storage.commit(&mut state).unwrap();

        assert!(observation.wrote);
        assert_eq!(committed_u64(&storage, 0), 1);
    }

    #[test]
    fn file_backed_rmw_flushes_and_removes_temporary_directory() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::FileBacked).unwrap();
        let paths = storage.file_paths.as_ref().unwrap();
        let directory = paths.directory.path().to_path_buf();
        let tmemory_path = paths.tmemory.clone();
        let mut state = storage.new_transaction_state().unwrap();

        storage.begin(&mut state).unwrap();
        let value = storage.read_u64(&mut state, 0).unwrap();
        storage.stage_u64(&mut state, 0, value + 1).unwrap();
        assert!(storage.commit(&mut state).unwrap().wrote);

        let bytes = std::fs::read(&tmemory_path).unwrap();
        assert_eq!(&bytes[..8], &1u64.to_le_bytes());
        drop(state);
        drop(storage);
        assert!(!directory.exists());
    }

    #[test]
    fn concurrent_rmw_conflicts_without_losing_an_update() {
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let ready = Arc::new(Barrier::new(2));

        let workers = (0..2)
            .map(|_| {
                let storage = storage.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    let _cleanup = clear_current_thread_transaction_on_drop_for_test();
                    let mut state = storage.new_transaction_state()?;
                    storage.begin(&mut state)?;
                    let value = storage.read_u64(&mut state, 0)?;
                    ready.wait();
                    let result = storage
                        .stage_u64(&mut state, 0, value + 1)
                        .map(|()| value + 1);
                    storage.finish_attempt(&mut state, result)
                })
            })
            .collect::<Vec<_>>();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap().unwrap())
            .collect::<Vec<_>>();

        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, AttemptOutcome::Conflict))
        );
        assert_eq!(committed_u64(&storage, 0), 1);
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn conflict_cleanup_failure_is_an_unexpected_attempt_error() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let mut stale = storage.new_transaction_state().unwrap();
        storage.begin(&mut stale).unwrap();
        assert_eq!(storage.read_u64(&mut stale, 0).unwrap(), 0);

        let committing_storage = storage.clone();
        std::thread::spawn(move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let mut state = committing_storage.new_transaction_state()?;
            committing_storage.begin(&mut state)?;
            committing_storage.stage_u64(&mut state, 0, 1)?;
            committing_storage.commit(&mut state)
        })
        .join()
        .unwrap()
        .unwrap();

        storage.stage_u64(&mut stale, 0, 2).unwrap();
        storage
            .runtime
            .visibility_for_test()
            .fail_finish_snapshot_once_for_test()
            .unwrap();

        let result = storage.finish_attempt(&mut stale, Ok(()));

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a conflict with failed snapshot cleanup was classified as Conflict"),
        };
        assert_eq!(stale.active_transaction(), None);
        assert!(format!("{error:#}").contains("injected snapshot finish failure"));
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn real_gc_barrier_removes_mvcc_versions_and_leaves_no_snapshots() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();

        for expected in 1..=4 {
            let mut state = storage.new_transaction_state().unwrap();
            storage.begin(&mut state).unwrap();
            let value = storage.read_u64(&mut state, 0).unwrap();
            storage.stage_u64(&mut state, 0, value + 1).unwrap();
            let observation = storage.commit(&mut state).unwrap();
            assert_eq!(observation.mvcc_versions_created, 1);
            assert_eq!(committed_u64(&storage, 0), expected);
        }

        let observation = storage.run_gc_barrier().unwrap();
        assert!(observation.barrier_removed_versions > 0);
        assert_eq!(observation.resident_versions_after, 0);
        assert_eq!(
            storage
                .runtime
                .visibility_for_test()
                .snapshot_lifecycle_counts_for_test()
                .unwrap()
                .0,
            0
        );
    }

    fn helper_rmw_result(backend: BackendKind) -> u64 {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(backend).unwrap();
        let mut state = storage.new_transaction_state().unwrap();
        storage.begin(&mut state).unwrap();
        let value = storage.read_u64(&mut state, 0).unwrap();
        storage.stage_u64(&mut state, 0, value + 1).unwrap();
        storage.commit(&mut state).unwrap();
        committed_u64(&storage, 0)
    }

    fn tfunc_rmw_result(backend: BackendKind) -> u64 {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let engine = crate::Engine::default();
        let module = crate::Module::new(
            &engine,
            wat::parse_str(
                r#"
                    (module
                      (tmemory 1)
                      (tfunc (export "increment")
                        (i32.tstore
                          (i32.const 0)
                          (i32.add (i32.tload (i32.const 0)) (i32.const 1))))
                      (tfunc (export "read") (result i32)
                        (i32.tload (i32.const 0))))
                "#,
            )
            .unwrap(),
        )
        .unwrap();
        let mut store = crate::Store::new(&engine, ());
        let _directory = if backend == BackendKind::FileBacked {
            let directory = tempfile::tempdir().unwrap();
            store
                .transaction_create_file_backed_storage_for_test(
                    directory.path().join("tmemory.bin"),
                    directory.path().join("transaction-log.bin"),
                    4096,
                )
                .unwrap();
            Some(directory)
        } else {
            None
        };
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let increment = instance
            .get_typed_func::<(), ()>(&mut store, "increment")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        increment.call(&mut store, ()).unwrap();
        u64::try_from(read.call(&mut store, ()).unwrap()).unwrap()
    }

    #[test]
    fn benchmark_helper_matches_real_tfunc_for_both_backends() {
        for backend in [BackendKind::Vmemory, BackendKind::FileBacked] {
            assert_eq!(helper_rmw_result(backend), tfunc_rmw_result(backend));
        }
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn mvcc_certification_does_not_hold_tmemory_mutex() {
        use crate::runtime::transaction::mvcc::MvccCommitTestHook;
        use std::sync::mpsc;
        use std::time::Duration;

        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let certification = Arc::new(MvccCommitTestHook::new(1));
        storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_predecessor_collected_hook_for_test(Some(certification.clone()))
            .unwrap();

        let committing_storage = storage.clone();
        let committing = std::thread::spawn(move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let mut state = committing_storage.new_transaction_state()?;
            committing_storage.begin(&mut state)?;
            let value = committing_storage.read_u64(&mut state, 0)?;
            committing_storage.stage_u64(&mut state, 0, value + 1)?;
            committing_storage.commit(&mut state)
        });

        assert!(certification.wait_until_reached(Duration::from_secs(5)));
        let probing_storage = storage.clone();
        let (probe_tx, probe_rx) = mpsc::channel();
        let probing = std::thread::spawn(move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let acquired = probing_storage.tmemory.try_lock().is_ok();
            probe_tx.send(acquired).unwrap();
        });
        let acquired = probe_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        certification.release();
        probing.join().unwrap();
        committing.join().unwrap().unwrap();
        storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_predecessor_collected_hook_for_test(None)
            .unwrap();

        assert!(acquired, "tmemory mutex was held across MVCC certification");
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn shared_file_backed_write_lock_excludes_mvcc_install_and_rollback() {
        use crate::runtime::transaction::MvccCommitFaultPoint;
        use crate::runtime::transaction::mvcc::MvccCommitTestHook;
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::Duration;

        let install_storage = BenchmarkStorage::new(BackendKind::FileBacked).unwrap();
        let install_hook = Arc::new(MvccCommitTestHook::new(1));
        install_storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_commit_hooks_for_test(None, None, Some(install_hook.clone()))
            .unwrap();
        let committing_storage = install_storage.clone();
        let committing = std::thread::spawn(move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let mut state = committing_storage.new_transaction_state()?;
            committing_storage.begin(&mut state)?;
            committing_storage.stage_u64(&mut state, 0, 1)?;
            committing_storage.commit(&mut state)
        });
        assert!(install_hook.wait_until_reached(Duration::from_secs(5)));

        let (writer_ready_tx, writer_ready_rx) = channel();
        let (writer_done_tx, writer_done_rx) = channel();
        let writer_runtime = install_storage.runtime.clone();
        let writer = std::thread::spawn(move || -> Result<()> {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            writer_ready_tx.send(()).unwrap();
            writer_runtime.with_shared_file_backed_tmemory_commit_write_lock(|| Ok(()))?;
            writer_done_tx.send(()).unwrap();
            Ok(())
        });
        writer_ready_rx.recv().unwrap();
        assert_eq!(
            writer_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        install_hook.release();
        assert!(committing.join().unwrap().unwrap().wrote);
        writer_done_rx.recv().unwrap();
        writer.join().unwrap().unwrap();
        install_storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_commit_hooks_for_test(None, None, None)
            .unwrap();

        let rollback_storage = BenchmarkStorage::new(BackendKind::FileBacked).unwrap();
        let rollback_hook = Arc::new(MvccCommitTestHook::new(1));
        rollback_storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_benchmark_rollback_hook_for_test(Some(rollback_hook.clone()))
            .unwrap();
        rollback_storage
            .runtime
            .visibility_for_test()
            .runtime()
            .fail_commit_once_for_test(MvccCommitFaultPoint::AfterMemoryInstall)
            .unwrap();
        let committing_storage = rollback_storage.clone();
        let committing = std::thread::spawn(move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let mut state = committing_storage.new_transaction_state()?;
            committing_storage.begin(&mut state)?;
            committing_storage.stage_u64(&mut state, 0, 1)?;
            committing_storage.commit(&mut state)
        });
        assert!(rollback_hook.wait_until_reached(Duration::from_secs(5)));

        let (writer_ready_tx, writer_ready_rx) = channel();
        let (writer_done_tx, writer_done_rx) = channel();
        let writer_runtime = rollback_storage.runtime.clone();
        let writer = std::thread::spawn(move || -> Result<()> {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            writer_ready_tx.send(()).unwrap();
            writer_runtime.with_shared_file_backed_tmemory_commit_write_lock(|| Ok(()))?;
            writer_done_tx.send(()).unwrap();
            Ok(())
        });
        writer_ready_rx.recv().unwrap();
        assert_eq!(
            writer_done_rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        );
        rollback_hook.release();
        let error = committing.join().unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("AfterMemoryInstall"));
        writer_done_rx.recv().unwrap();
        writer.join().unwrap().unwrap();
        rollback_storage
            .runtime
            .visibility_for_test()
            .runtime()
            .set_benchmark_rollback_hook_for_test(None)
            .unwrap();
        assert_eq!(committed_u64(&rollback_storage, 0), 0);
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn mvcc_pre_lp_install_failure_rolls_back_current_value_for_both_backends() {
        use crate::runtime::transaction::MvccCommitFaultPoint;

        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        for backend in [BackendKind::Vmemory, BackendKind::FileBacked] {
            let storage = BenchmarkStorage::new(backend).unwrap();
            storage
                .runtime
                .visibility_for_test()
                .runtime()
                .fail_commit_once_for_test(MvccCommitFaultPoint::AfterMemoryInstall)
                .unwrap();
            let mut state = storage.new_transaction_state().unwrap();
            storage.begin(&mut state).unwrap();
            storage.stage_u64(&mut state, 0, 1).unwrap();

            let error = storage.commit(&mut state).unwrap_err();

            assert!(
                format!("{error:#}").contains("AfterMemoryInstall"),
                "{error:#}"
            );
            assert_eq!(state.active_transaction(), None);
            assert_eq!(committed_u64(&storage, 0), 0);
        }
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn mvcc_post_lp_failure_retains_retryable_force_completion() {
        use crate::runtime::transaction::MvccCommitFaultPoint;

        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::FileBacked).unwrap();
        let visibility = storage.runtime.visibility_for_test();
        visibility
            .runtime()
            .fail_commit_once_for_test(MvccCommitFaultPoint::AfterDurableLp)
            .unwrap();
        visibility
            .runtime()
            .fail_commit_once_for_test(MvccCommitFaultPoint::BeforeRecordPublication)
            .unwrap();
        let mut state = storage.new_transaction_state().unwrap();
        storage.begin(&mut state).unwrap();
        storage.stage_u64(&mut state, 0, 1).unwrap();

        let error = storage.commit(&mut state).unwrap_err();
        assert!(format!("{error:#}").contains("committed durably"));
        assert!(state.has_benchmark_mvcc_terminal_for_test());
        assert_eq!(storage.mvcc_versions_created.load(Ordering::Relaxed), 0);

        let observation = storage.commit(&mut state).unwrap();
        assert!(observation.wrote);
        assert_eq!(observation.mvcc_versions_created, 1);
        assert_eq!(storage.mvcc_versions_created.load(Ordering::Relaxed), 1);
        assert!(!state.has_benchmark_mvcc_terminal_for_test());
        assert_eq!(state.active_transaction(), None);
        assert_eq!(committed_u64(&storage, 0), 1);
    }

    #[cfg(all(
        feature = "transaction-mvcc",
        feature = "transaction-cc-optimistic-validation"
    ))]
    #[test]
    fn mvcc_synchronous_post_lp_force_completion_counts_versions_once() {
        use crate::runtime::transaction::MvccCommitFaultPoint;

        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::FileBacked).unwrap();
        storage
            .runtime
            .visibility_for_test()
            .runtime()
            .fail_commit_once_for_test(MvccCommitFaultPoint::AfterDurableLp)
            .unwrap();
        let mut state = storage.new_transaction_state().unwrap();
        storage.begin(&mut state).unwrap();
        storage.stage_u64(&mut state, 0, 1).unwrap();

        let error = storage.commit(&mut state).unwrap_err();

        assert!(format!("{error:#}").contains("committed durably"));
        assert_eq!(state.active_transaction(), None);
        assert!(!state.has_benchmark_mvcc_terminal_for_test());
        assert_eq!(storage.mvcc_versions_created.load(Ordering::Relaxed), 1);
        assert_eq!(committed_u64(&storage, 0), 1);
        assert_eq!(storage.mvcc_versions_created.load(Ordering::Relaxed), 1);
    }
}
