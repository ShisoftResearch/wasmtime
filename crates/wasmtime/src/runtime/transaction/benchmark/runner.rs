use super::config::{BenchmarkRequest, CellSpec, WorkloadKind};
use super::metrics::{CellAggregateInput, CellMetrics, GcMetrics, VersionMetrics, WorkerMetrics};
use super::record::{CellRecord, FailureRecord, SCHEMA_VERSION};
use super::storage::{
    AttemptOutcome, BenchmarkStorage, GcBarrierObservation, VersionCounterSnapshot,
};
use super::workload::{
    DeterministicRng, MeasurementBaseline, OperationEffect, WorkerContext, WorkloadState,
    read_committed_u64s, write_skew_addresses,
};
use crate::prelude::*;
use crate::runtime::transaction::clear_current_thread_transaction_on_drop_for_test;
use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct WriteSkewPair {
    addresses: [u64; 2],
    rendezvous: Arc<CancellableRendezvous>,
}

struct RendezvousState {
    generation: u64,
    arrivals: usize,
    broken: bool,
}

pub(super) struct CancellableRendezvous {
    participants: usize,
    state: Mutex<RendezvousState>,
    changed: Condvar,
    cancelled: Arc<AtomicBool>,
}

impl CancellableRendezvous {
    pub(super) fn new(participants: usize, cancelled: Arc<AtomicBool>) -> Self {
        assert!(participants > 0);
        Self {
            participants,
            state: Mutex::new(RendezvousState {
                generation: 0,
                arrivals: 0,
                broken: false,
            }),
            changed: Condvar::new(),
            cancelled,
        }
    }

    pub(super) fn wait(&self, deadline: Instant) -> Result<()> {
        self.wait_for_leader(deadline).map(|_| ())
    }

    fn wait_for_leader(&self, deadline: Instant) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| crate::format_err!("benchmark rendezvous lock is poisoned"))?;
        if state.broken || self.cancelled.load(Ordering::Acquire) {
            bail!("benchmark rendezvous cancelled");
        }
        let generation = state.generation;
        state.arrivals += 1;
        if state.arrivals == self.participants {
            state.arrivals = 0;
            state.generation = state
                .generation
                .checked_add(1)
                .context("benchmark rendezvous generation overflow")?;
            self.changed.notify_all();
            return Ok(true);
        }

        loop {
            if state.broken || self.cancelled.load(Ordering::Acquire) {
                bail!("benchmark rendezvous cancelled");
            }
            if state.generation != generation {
                return Ok(false);
            }
            let now = Instant::now();
            if now >= deadline {
                state.broken = true;
                state.arrivals = 0;
                self.changed.notify_all();
                bail!("benchmark rendezvous timed out");
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next, timeout) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| crate::format_err!("benchmark rendezvous lock is poisoned"))?;
            state = next;
            if timeout.timed_out() && state.generation == generation {
                state.broken = true;
                state.arrivals = 0;
                self.changed.notify_all();
                bail!("benchmark rendezvous timed out");
            }
        }
    }

    pub(super) fn break_all(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.broken = true;
            state.arrivals = 0;
            self.changed.notify_all();
        }
    }
}

#[derive(Default)]
struct PhaseTimes {
    warmup_deadline: Option<Instant>,
    measurement_started: Option<Instant>,
    measurement_deadline: Option<Instant>,
}

struct CellPhases {
    rendezvous: Arc<CancellableRendezvous>,
    times: Mutex<PhaseTimes>,
}

impl CellPhases {
    fn new(participants: usize, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            rendezvous: Arc::new(CancellableRendezvous::new(participants, cancelled)),
            times: Mutex::new(PhaseTimes::default()),
        }
    }

    fn rendezvous(&self) -> Arc<CancellableRendezvous> {
        self.rendezvous.clone()
    }

    fn begin_warmup(&self, duration: Duration, watchdog_deadline: Instant) -> Result<Instant> {
        let leader = self.rendezvous.wait_for_leader(watchdog_deadline)?;
        if leader {
            self.times
                .lock()
                .map_err(|_| crate::format_err!("benchmark phase timing lock is poisoned"))?
                .warmup_deadline = Some(Instant::now() + duration);
        }
        self.rendezvous.wait(watchdog_deadline)?;
        self.times
            .lock()
            .map_err(|_| crate::format_err!("benchmark phase timing lock is poisoned"))?
            .warmup_deadline
            .context("benchmark warmup deadline was not initialized")
    }

    fn begin_measurement(
        &self,
        duration: Duration,
        watchdog_deadline: Instant,
        prepare: impl FnOnce() -> Result<()>,
    ) -> Result<Instant> {
        let leader = self.rendezvous.wait_for_leader(watchdog_deadline)?;
        if leader {
            prepare()?;
            let started = Instant::now();
            let mut times = self
                .times
                .lock()
                .map_err(|_| crate::format_err!("benchmark phase timing lock is poisoned"))?;
            times.measurement_started = Some(started);
            times.measurement_deadline = Some(started + duration);
        }
        self.rendezvous.wait(watchdog_deadline)?;
        self.times
            .lock()
            .map_err(|_| crate::format_err!("benchmark phase timing lock is poisoned"))?
            .measurement_deadline
            .context("benchmark measurement deadline was not initialized")
    }

    fn measurement_elapsed(&self) -> Result<Duration> {
        Ok(self
            .times
            .lock()
            .map_err(|_| crate::format_err!("benchmark phase timing lock is poisoned"))?
            .measurement_started
            .context("benchmark measurement never started")?
            .elapsed())
    }
}

#[derive(Default)]
struct MaintenanceState {
    write_commits: u64,
    next_barrier: u64,
    gc: GcMetrics,
    barrier_pruned: u64,
}

pub(super) struct MaintenanceGate {
    attempts: RwLock<()>,
    maintenance: Mutex<MaintenanceState>,
    storage: Arc<BenchmarkStorage>,
    interval: u64,
}

impl MaintenanceGate {
    fn new(storage: Arc<BenchmarkStorage>, interval: u64) -> Result<Self> {
        ensure!(interval > 0, "GC commit interval must be nonzero");
        Ok(Self {
            attempts: RwLock::new(()),
            maintenance: Mutex::new(MaintenanceState {
                next_barrier: interval,
                ..MaintenanceState::default()
            }),
            storage,
            interval,
        })
    }

    fn attempt_guard(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.attempts
            .read()
            .map_err(|_| crate::format_err!("benchmark maintenance gate is poisoned"))
    }

    pub(super) fn after_write_commit(&self) -> Result<()> {
        let mut state = self
            .maintenance
            .lock()
            .map_err(|_| crate::format_err!("benchmark maintenance state is poisoned"))?;
        state.write_commits = state
            .write_commits
            .checked_add(1)
            .context("benchmark write-commit cadence overflow")?;
        if state.write_commits < state.next_barrier {
            return Ok(());
        }
        let crossed = state.write_commits / self.interval;
        state.next_barrier = crossed
            .checked_add(1)
            .and_then(|multiple| multiple.checked_mul(self.interval))
            .context("benchmark GC cadence overflow")?;
        self.record_barrier(&mut state)
    }

    fn warmup_barrier(&self) -> Result<()> {
        let mut state = self
            .maintenance
            .lock()
            .map_err(|_| crate::format_err!("benchmark maintenance state is poisoned"))?;
        let _exclusive = self
            .attempts
            .write()
            .map_err(|_| crate::format_err!("benchmark maintenance gate is poisoned"))?;
        self.storage.run_gc_barrier()?;
        *state = MaintenanceState {
            next_barrier: self.interval,
            ..MaintenanceState::default()
        };
        Ok(())
    }

    pub(super) fn final_barrier(&self) -> Result<()> {
        let mut state = self
            .maintenance
            .lock()
            .map_err(|_| crate::format_err!("benchmark maintenance state is poisoned"))?;
        self.record_barrier(&mut state)
    }

    fn record_barrier(&self, state: &mut MaintenanceState) -> Result<()> {
        let _exclusive = self
            .attempts
            .write()
            .map_err(|_| crate::format_err!("benchmark maintenance gate is poisoned"))?;
        let observation = self.storage.run_gc_barrier()?;
        merge_gc_observation(state, &observation)
    }

    fn metrics(&self) -> Result<(GcMetrics, u64)> {
        let state = self
            .maintenance
            .lock()
            .map_err(|_| crate::format_err!("benchmark maintenance state is poisoned"))?;
        Ok((state.gc.clone(), state.barrier_pruned))
    }
}

fn merge_gc_observation(
    state: &mut MaintenanceState,
    observation: &GcBarrierObservation,
) -> Result<()> {
    let elapsed_ns = u64::try_from(observation.elapsed.as_nanos()).unwrap_or(u64::MAX);
    state.gc.barriers = state
        .gc
        .barriers
        .checked_add(1)
        .context("benchmark GC barrier counter overflow")?;
    state.gc.total_ns = state
        .gc
        .total_ns
        .checked_add(elapsed_ns)
        .context("benchmark GC elapsed counter overflow")?;
    state.gc.max_pause_ns = state.gc.max_pause_ns.max(elapsed_ns);
    if state.gc.policy_mode.is_empty() {
        state.gc.policy_mode = observation.policy_mode.to_string();
    } else {
        ensure!(
            state.gc.policy_mode == observation.policy_mode,
            "benchmark GC policy mode changed from {} to {}",
            state.gc.policy_mode,
            observation.policy_mode
        );
    }
    state.barrier_pruned = state
        .barrier_pruned
        .checked_add(observation.barrier_removed_versions)
        .context("benchmark barrier-pruned version counter overflow")?;
    let _ = (
        observation.resident_versions_before,
        observation.resident_versions_after,
        observation.opportunistically_pruned_versions,
    );
    Ok(())
}

pub(super) fn retry_delay(rng: &mut DeterministicRng, consecutive_aborts: u32) -> Duration {
    let ceiling = 1u64 << consecutive_aborts.min(6);
    Duration::from_micros(1 + rng.next_u64() % ceiling.min(64))
}

pub(super) fn validate_write_skew_pair(left: u64, right: u64) -> Result<()> {
    ensure!(
        left <= 1 && right <= 1,
        "write-skew values must be zero or one"
    );
    ensure!(
        (left, right) != (0, 0),
        "write-skew invariant violated: observed (0,0)"
    );
    Ok(())
}

pub(super) fn run_write_skew_attempt(
    pair: &WriteSkewPair,
    worker: &mut WorkerContext,
) -> Result<AttemptOutcome<OperationEffect>> {
    let storage = worker.storage.clone();
    storage.begin(&mut worker.state)?;
    let result = (|| {
        let values = [
            storage.read_u64(&mut worker.state, pair.addresses[0])?,
            storage.read_u64(&mut worker.state, pair.addresses[1])?,
        ];
        if worker.write_skew_initial_attempt {
            worker.write_skew_initial_attempt = false;
            let deadline = worker
                .rendezvous_deadline
                .context("write-skew attempt has no rendezvous deadline")?;
            pair.rendezvous.wait(deadline)?;
        }
        let role = worker.write_skew_role;
        ensure!(role < 2, "write-skew role must be zero or one");
        let other = 1 - role;
        let writes = if values[other] == 1 {
            storage.stage_u64(&mut worker.state, pair.addresses[role], 0)?;
            1
        } else {
            0
        };
        Ok(OperationEffect {
            checksum: values[0].wrapping_add(values[1]),
            writes,
            counter_delta: 0,
        })
    })();
    storage.finish_attempt(&mut worker.state, result)
}

fn run_restore_attempt(
    pair: &WriteSkewPair,
    worker: &mut WorkerContext,
) -> Result<AttemptOutcome<OperationEffect>> {
    let storage = worker.storage.clone();
    storage.begin(&mut worker.state)?;
    let result = (|| {
        storage.stage_u64(&mut worker.state, pair.addresses[0], 1)?;
        storage.stage_u64(&mut worker.state, pair.addresses[1], 1)?;
        Ok(OperationEffect {
            checksum: 0,
            writes: 2,
            counter_delta: 0,
        })
    })();
    storage.finish_attempt(&mut worker.state, result)
}

fn retry_logical_operation(
    worker: &mut WorkerContext,
    metrics: &mut WorkerMetrics,
    maintenance: &MaintenanceGate,
    cancelled: &AtomicBool,
    mut attempt: impl FnMut(&mut WorkerContext) -> Result<AttemptOutcome<OperationEffect>>,
) -> Result<OperationEffect> {
    let logical_started = Instant::now();
    let mut consecutive_aborts = 0u32;
    loop {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "benchmark cell cancelled during logical operation"
        );
        let outcome = {
            let _attempt_guard = maintenance.attempt_guard()?;
            attempt(worker)?
        };
        match outcome {
            AttemptOutcome::Committed(effect) => {
                metrics.record_commit(logical_started.elapsed())?;
                consecutive_aborts = 0;
                if effect.writes != 0 {
                    maintenance.after_write_commit()?;
                }
                let _ = consecutive_aborts;
                return Ok(effect);
            }
            AttemptOutcome::Conflict => {
                metrics.record_conflict()?;
                let delay = retry_delay(&mut worker.rng, consecutive_aborts);
                consecutive_aborts = consecutive_aborts.saturating_add(1);
                ensure!(
                    !cancelled.load(Ordering::Acquire),
                    "benchmark cell cancelled before retry"
                );
                thread::sleep(delay);
            }
        }
    }
}

#[derive(Debug, Default)]
struct WorkerReport {
    index: usize,
    metrics: WorkerMetrics,
    restore: WorkerMetrics,
    schedules: u64,
    observed_pairs: Vec<[u64; 2]>,
    final_pairs: Vec<[u64; 2]>,
}

type WorkerFn = Box<dyn FnOnce() -> Result<WorkerReport> + Send + 'static>;

struct WorkerJob {
    name: String,
    progress: Arc<AtomicU64>,
    run: WorkerFn,
}

impl WorkerJob {
    fn injected(
        name: impl Into<String>,
        progress: Arc<AtomicU64>,
        run: impl FnOnce() -> Result<WorkerReport> + Send + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            progress,
            run: Box::new(run),
        }
    }
}

enum WorkerOutcome {
    Completed(WorkerReport),
    Failed(String),
    Panicked(String),
}

struct WorkerEvent {
    name: String,
    outcome: WorkerOutcome,
}

struct RunningWorker {
    name: String,
    progress: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

fn spawn_jobs(jobs: Vec<WorkerJob>) -> Result<(Vec<RunningWorker>, Receiver<WorkerEvent>)> {
    let (sender, receiver) = mpsc::channel();
    let mut running = Vec::with_capacity(jobs.len());
    for job in jobs {
        let name = job.name;
        let thread_name = name.clone();
        let event_name = name.clone();
        let progress = job.progress;
        let progress_for_handle = progress.clone();
        let sender = sender.clone();
        let run = job.run;
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let outcome = match catch_unwind(AssertUnwindSafe(run)) {
                    Ok(Ok(report)) => WorkerOutcome::Completed(report),
                    Ok(Err(error)) => WorkerOutcome::Failed(format!("{error:#}")),
                    Err(payload) => WorkerOutcome::Panicked(panic_payload(payload)),
                };
                let _ = sender.send(WorkerEvent {
                    name: event_name,
                    outcome,
                });
            })
            .context("failed to spawn benchmark worker")?;
        running.push(RunningWorker {
            name,
            progress: progress_for_handle,
            handle: Some(handle),
        });
    }
    drop(sender);
    Ok((running, receiver))
}

fn panic_payload(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn supervise_jobs(
    spec: &CellSpec,
    phase: &str,
    jobs: Vec<WorkerJob>,
    watchdog_deadline: Instant,
    cancelled: Arc<AtomicBool>,
    rendezvous: &[Arc<CancellableRendezvous>],
) -> Result<Vec<WorkerReport>> {
    let (mut running, receiver) = spawn_jobs(jobs)?;
    let expected = running.len();
    let mut reports = Vec::with_capacity(expected);
    let mut primary_failure = None;

    while reports.len() < expected && primary_failure.is_none() {
        let now = Instant::now();
        if now >= watchdog_deadline {
            primary_failure = Some("cell watchdog timed out".to_string());
            break;
        }
        match receiver.recv_timeout(watchdog_deadline.saturating_duration_since(now)) {
            Ok(event) => match event.outcome {
                WorkerOutcome::Completed(report) => reports.push(report),
                WorkerOutcome::Failed(message) => {
                    primary_failure = Some(format!("worker {} failed: {message}", event.name));
                }
                WorkerOutcome::Panicked(message) => {
                    primary_failure = Some(format!("worker {} panicked: {message}", event.name));
                }
            },
            Err(RecvTimeoutError::Timeout) => {
                primary_failure = Some("cell watchdog timed out".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => {
                primary_failure = Some("worker supervisor channel disconnected".to_string());
            }
        }
    }

    if let Some(failure) = primary_failure {
        cancel_workers(&cancelled, rendezvous);
        drain_finished_workers(&mut running, watchdog_deadline);
        let progress = worker_progress(&running);
        let unfinished = unfinished_workers(&running);
        return Err(cell_failure(
            spec,
            phase,
            format!("{failure}; unfinished workers: {unfinished}; last progress: {progress}"),
        ));
    }

    drain_finished_workers(&mut running, watchdog_deadline);
    let unfinished = unfinished_workers(&running);
    if unfinished != "none" {
        cancel_workers(&cancelled, rendezvous);
        return Err(cell_failure(
            spec,
            phase,
            format!(
                "workers reported completion but did not terminate: {unfinished}; last progress: {}",
                worker_progress(&running)
            ),
        ));
    }
    Ok(reports)
}

fn cancel_workers(cancelled: &AtomicBool, rendezvous: &[Arc<CancellableRendezvous>]) {
    cancelled.store(true, Ordering::Release);
    for rendezvous in rendezvous {
        rendezvous.break_all();
    }
}

fn drain_finished_workers(running: &mut [RunningWorker], deadline: Instant) {
    loop {
        for worker in running.iter_mut() {
            let finished = worker.handle.as_ref().is_some_and(JoinHandle::is_finished);
            if finished {
                if let Some(handle) = worker.handle.take() {
                    let _ = handle.join();
                }
            }
        }
        if running.iter().all(|worker| worker.handle.is_none()) || Instant::now() >= deadline {
            return;
        }
        thread::yield_now();
    }
}

fn worker_progress(running: &[RunningWorker]) -> String {
    running
        .iter()
        .map(|worker| {
            format!(
                "{}={}",
                worker.name,
                worker.progress.load(Ordering::Acquire)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn unfinished_workers(running: &[RunningWorker]) -> String {
    let unfinished = running
        .iter()
        .filter(|worker| worker.handle.is_some())
        .map(|worker| worker.name.clone())
        .collect::<Vec<_>>();
    if unfinished.is_empty() {
        "none".to_string()
    } else {
        unfinished.join(", ")
    }
}

fn cell_failure(spec: &CellSpec, phase: &str, message: String) -> crate::Error {
    let failure = FailureRecord {
        schema_version: SCHEMA_VERSION,
        policy: spec.policy.clone(),
        cell: Some(spec.clone()),
        phase: phase.to_string(),
        message,
    };
    crate::format_err!(
        "transaction benchmark failure in cell {} during phase {}: {}",
        cell_name(spec),
        failure.phase,
        failure.message
    )
}

fn cell_name(spec: &CellSpec) -> String {
    format!(
        "{}/{}/{}/{}-workers/repetition-{}",
        spec.policy,
        match spec.backend {
            super::config::BackendKind::Vmemory => "vmemory",
            super::config::BackendKind::FileBacked => "file-backed",
        },
        match spec.workload {
            WorkloadKind::ReadOnly => "read-only",
            WorkloadKind::DisjointWrites => "disjoint-writes",
            WorkloadKind::HotKey => "hot-key",
            WorkloadKind::WriteSkew => "write-skew",
        },
        spec.workers,
        spec.repetition
    )
}

fn supervise_test_jobs(
    spec: &CellSpec,
    phase: &str,
    jobs: Vec<WorkerJob>,
    requested: Duration,
    grace: Duration,
    cancelled: Arc<AtomicBool>,
    rendezvous: Vec<Arc<CancellableRendezvous>>,
) -> Result<Vec<WorkerReport>> {
    supervise_jobs(
        spec,
        phase,
        jobs,
        Instant::now() + requested + grace,
        cancelled,
        &rendezvous,
    )
}

enum PhaseLimit {
    Deadline(Instant),
    Schedules(u64),
}

fn run_standard_phase(
    workload: &WorkloadState,
    worker: &mut WorkerContext,
    maintenance: &MaintenanceGate,
    cancelled: &AtomicBool,
    progress: &AtomicU64,
    deadline: Instant,
) -> Result<WorkerReport> {
    let mut report = WorkerReport::default();
    while Instant::now() < deadline && !cancelled.load(Ordering::Acquire) {
        let sequence = worker.sequence;
        retry_logical_operation(
            worker,
            &mut report.metrics,
            maintenance,
            cancelled,
            |worker| workload.execute_attempt(worker, sequence),
        )?;
        worker.sequence = worker
            .sequence
            .checked_add(1)
            .context("benchmark logical-operation sequence overflow")?;
        progress.fetch_add(1, Ordering::Release);
    }
    Ok(report)
}

fn run_write_skew_phase(
    pair: &WriteSkewPair,
    worker: &mut WorkerContext,
    maintenance: &MaintenanceGate,
    cancelled: &AtomicBool,
    progress: &AtomicU64,
    limit: PhaseLimit,
    continue_schedule: &AtomicBool,
    rendezvous_deadline: Instant,
) -> Result<WorkerReport> {
    let mut report = WorkerReport::default();
    let role = worker.write_skew_role;
    let mut completed = 0u64;
    loop {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "benchmark cell cancelled before write-skew schedule"
        );
        worker.begin_write_skew_operation(role, rendezvous_deadline);
        retry_logical_operation(
            worker,
            &mut report.metrics,
            maintenance,
            cancelled,
            |worker| run_write_skew_attempt(pair, worker),
        )?;
        worker.sequence = worker
            .sequence
            .checked_add(1)
            .context("benchmark logical-operation sequence overflow")?;
        progress.fetch_add(1, Ordering::Release);

        pair.rendezvous.wait(rendezvous_deadline)?;
        if role == 0 {
            let observed = read_pair_with_gate(pair, worker, maintenance)?;
            validate_write_skew_pair(observed[0], observed[1])?;
            report.observed_pairs.push(observed);
            report.schedules = report
                .schedules
                .checked_add(1)
                .context("write-skew schedule counter overflow")?;
            completed = completed
                .checked_add(1)
                .context("write-skew schedule counter overflow")?;
            retry_logical_operation(
                worker,
                &mut report.restore,
                maintenance,
                cancelled,
                |worker| run_restore_attempt(pair, worker),
            )?;
            let restored = read_pair_with_gate(pair, worker, maintenance)?;
            ensure!(
                restored == [1, 1],
                "write-skew restore produced ({},{})",
                restored[0],
                restored[1]
            );
            report.final_pairs.push(restored);
            let keep_running = match limit {
                PhaseLimit::Deadline(deadline) => Instant::now() < deadline,
                PhaseLimit::Schedules(schedules) => completed < schedules,
            };
            continue_schedule.store(keep_running, Ordering::Release);
        }
        pair.rendezvous.wait(rendezvous_deadline)?;
        if !continue_schedule.load(Ordering::Acquire) {
            break;
        }
    }
    Ok(report)
}

fn run_write_skew_control_phase(
    pair: &WriteSkewPair,
    worker: &mut WorkerContext,
    maintenance: &MaintenanceGate,
    cancelled: &AtomicBool,
    progress: &AtomicU64,
    limit: PhaseLimit,
    rendezvous_deadline: Instant,
) -> Result<WorkerReport> {
    let mut report = WorkerReport::default();
    loop {
        if matches!(limit, PhaseLimit::Deadline(deadline) if Instant::now() >= deadline) {
            break;
        }
        for role in 0..2 {
            worker.begin_write_skew_operation(role, rendezvous_deadline);
            retry_logical_operation(
                worker,
                &mut report.metrics,
                maintenance,
                cancelled,
                |worker| run_write_skew_attempt(pair, worker),
            )?;
            worker.sequence = worker
                .sequence
                .checked_add(1)
                .context("benchmark logical-operation sequence overflow")?;
            progress.fetch_add(1, Ordering::Release);
        }
        let observed = read_pair_with_gate(pair, worker, maintenance)?;
        validate_write_skew_pair(observed[0], observed[1])?;
        report.observed_pairs.push(observed);
        report.schedules = report
            .schedules
            .checked_add(1)
            .context("write-skew schedule counter overflow")?;
        retry_logical_operation(
            worker,
            &mut report.restore,
            maintenance,
            cancelled,
            |worker| run_restore_attempt(pair, worker),
        )?;
        let restored = read_pair_with_gate(pair, worker, maintenance)?;
        ensure!(
            restored == [1, 1],
            "write-skew restore did not restore (1,1)"
        );
        report.final_pairs.push(restored);
        if matches!(limit, PhaseLimit::Schedules(schedules) if report.schedules >= schedules) {
            break;
        }
    }
    Ok(report)
}

fn read_pair_with_gate(
    pair: &WriteSkewPair,
    worker: &WorkerContext,
    maintenance: &MaintenanceGate,
) -> Result<[u64; 2]> {
    let _attempt_guard = maintenance.attempt_guard()?;
    read_committed_u64s(&worker.storage, pair.addresses)
}

fn merge_report(target: &mut WorkerReport, source: WorkerReport) -> Result<()> {
    target.metrics.merge(&source.metrics)?;
    target.restore.merge(&source.restore)?;
    target.schedules = target
        .schedules
        .checked_add(source.schedules)
        .context("write-skew schedule counter overflow")?;
    target.observed_pairs.extend(source.observed_pairs);
    target.final_pairs.extend(source.final_pairs);
    Ok(())
}

fn make_write_skew_pairs(
    workers: usize,
    cancelled: &Arc<AtomicBool>,
) -> Result<Vec<Arc<WriteSkewPair>>> {
    let pair_count = if workers == 1 { 1 } else { workers.div_ceil(2) };
    ensure!(
        workers == 1 || workers % 2 == 0,
        "parallel write-skew cells require an even worker count"
    );
    (0..pair_count)
        .map(|pair| {
            let participants = if workers == 1 { 1 } else { 2 };
            Ok(Arc::new(WriteSkewPair {
                addresses: write_skew_addresses(pair)?,
                rendezvous: Arc::new(CancellableRendezvous::new(participants, cancelled.clone())),
            }))
        })
        .collect()
}

fn rendezvous_list(
    pairs: &[Arc<WriteSkewPair>],
    phase: Option<Arc<CancellableRendezvous>>,
) -> Vec<Arc<CancellableRendezvous>> {
    let mut rendezvous = pairs
        .iter()
        .map(|pair| pair.rendezvous.clone())
        .collect::<Vec<_>>();
    rendezvous.extend(phase);
    rendezvous
}

pub(super) fn run_cell(request: &BenchmarkRequest, spec: &CellSpec) -> Result<CellRecord> {
    request.validate()?;
    ensure!(
        spec.workers > 0,
        "benchmark cell worker count must be nonzero"
    );
    let storage = BenchmarkStorage::new(spec.backend)?;
    let workload = Arc::new(WorkloadState::initialize(&storage, spec)?);
    let maintenance = Arc::new(MaintenanceGate::new(
        storage.clone(),
        request.gc_commit_interval,
    )?);
    let cancelled = Arc::new(AtomicBool::new(false));
    let pairs = if spec.workload == WorkloadKind::WriteSkew {
        make_write_skew_pairs(spec.workers, &cancelled)?
    } else {
        Vec::new()
    };
    let pair_continuations = (0..pairs.len())
        .map(|_| Arc::new(AtomicBool::new(true)))
        .collect::<Vec<_>>();
    let phases = Arc::new(CellPhases::new(spec.workers, cancelled.clone()));
    let baseline = Arc::new(Mutex::new(None));
    let version_start = Arc::new(Mutex::new(None));
    let cell_started = Instant::now();
    let warmup = Duration::from_millis(request.warmup_ms);
    let measurement = Duration::from_millis(request.measure_ms);
    let requested = warmup + measurement;
    let grace = Duration::from_secs(5).max(requested.saturating_mul(2));
    let watchdog_deadline = cell_started + requested + grace;
    let mut jobs = Vec::with_capacity(spec.workers);
    let worker_count = spec.workers;

    for index in 0..spec.workers {
        let name = format!("worker-{index}");
        let progress = Arc::new(AtomicU64::new(0));
        let progress_for_job = progress.clone();
        let storage = storage.clone();
        let workload = workload.clone();
        let maintenance = maintenance.clone();
        let cancelled_for_job = cancelled.clone();
        let phases = phases.clone();
        let baseline = baseline.clone();
        let version_start = version_start.clone();
        let pair = pairs.get(index / 2).cloned();
        let pair_continue = pair_continuations.get(index / 2).cloned();
        let seed = spec.seed;
        let workload_kind = spec.workload;
        jobs.push(WorkerJob::injected(name, progress, move || {
            let _cleanup = clear_current_thread_transaction_on_drop_for_test();
            let mut worker = WorkerContext::new(&storage, seed, index)?;
            let warmup_deadline = phases.begin_warmup(warmup, watchdog_deadline)?;
            let warmup_report = match workload_kind {
                WorkloadKind::WriteSkew if worker_count == 1 => run_write_skew_control_phase(
                    pair.as_ref().unwrap(),
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    PhaseLimit::Deadline(warmup_deadline),
                    watchdog_deadline,
                )?,
                WorkloadKind::WriteSkew => run_write_skew_phase(
                    pair.as_ref().unwrap(),
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    PhaseLimit::Deadline(warmup_deadline),
                    pair_continue.as_ref().unwrap(),
                    watchdog_deadline,
                )?,
                _ => run_standard_phase(
                    &workload,
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    warmup_deadline,
                )?,
            };
            drop(warmup_report);
            let deadline = phases.begin_measurement(measurement, watchdog_deadline, || {
                maintenance.warmup_barrier()?;
                let _attempt_guard = maintenance.attempt_guard()?;
                *baseline
                    .lock()
                    .map_err(|_| crate::format_err!("benchmark baseline lock is poisoned"))? =
                    Some(workload.capture_baseline(&storage)?);
                *version_start.lock().map_err(|_| {
                    crate::format_err!("benchmark version baseline lock is poisoned")
                })? = Some(storage.version_counter_snapshot()?);
                Ok(())
            })?;
            let mut report = match workload_kind {
                WorkloadKind::WriteSkew if worker_count == 1 => run_write_skew_control_phase(
                    pair.as_ref().unwrap(),
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    PhaseLimit::Deadline(deadline),
                    watchdog_deadline,
                )?,
                WorkloadKind::WriteSkew => run_write_skew_phase(
                    pair.as_ref().unwrap(),
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    PhaseLimit::Deadline(deadline),
                    pair_continue.as_ref().unwrap(),
                    watchdog_deadline,
                )?,
                _ => run_standard_phase(
                    &workload,
                    &mut worker,
                    &maintenance,
                    &cancelled_for_job,
                    &progress_for_job,
                    deadline,
                )?,
            };
            report.index = index;
            ensure!(
                worker.state.active_transaction().is_none(),
                "worker {index} leaked an active transaction"
            );
            Ok(report)
        }));
    }

    let reports = supervise_jobs(
        spec,
        "warmup-and-measurement",
        jobs,
        watchdog_deadline,
        cancelled,
        &rendezvous_list(&pairs, Some(phases.rendezvous())),
    )?;
    maintenance.final_barrier()?;
    let effective_elapsed = phases.measurement_elapsed()?;
    let baseline = baseline
        .lock()
        .map_err(|_| crate::format_err!("benchmark baseline lock is poisoned"))?
        .clone()
        .context("benchmark baseline was not captured")?;
    let version_start = version_start
        .lock()
        .map_err(|_| crate::format_err!("benchmark version baseline lock is poisoned"))?
        .context("benchmark version baseline was not captured")?;
    finish_cell(
        spec,
        &storage,
        &workload,
        &maintenance,
        &baseline,
        version_start,
        reports,
        measurement,
        effective_elapsed,
    )
    .map(|outcome| outcome.record)
}

struct FixedWriteSkewOutcome {
    record: CellRecord,
    schedules: u64,
    observed_pairs: Vec<[u64; 2]>,
    final_pairs: Vec<[u64; 2]>,
}

fn run_fixed_write_skew_cell(
    request: &BenchmarkRequest,
    spec: &CellSpec,
    schedules: u64,
) -> Result<FixedWriteSkewOutcome> {
    ensure!(spec.workload == WorkloadKind::WriteSkew);
    ensure!(
        schedules > 0,
        "fixed write-skew schedule count must be nonzero"
    );
    let storage = BenchmarkStorage::new(spec.backend)?;
    let workload = WorkloadState::initialize(&storage, spec)?;
    let maintenance = Arc::new(MaintenanceGate::new(
        storage.clone(),
        request.gc_commit_interval,
    )?);
    maintenance.warmup_barrier()?;
    let (baseline, version_start) = {
        let _attempt_guard = maintenance.attempt_guard()?;
        (
            workload.capture_baseline(&storage)?,
            storage.version_counter_snapshot()?,
        )
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let pairs = make_write_skew_pairs(spec.workers, &cancelled)?;
    let continuations = (0..pairs.len())
        .map(|_| Arc::new(AtomicBool::new(true)))
        .collect::<Vec<_>>();
    let start = Arc::new(CancellableRendezvous::new(spec.workers, cancelled.clone()));
    let measurement_started = Arc::new(Mutex::new(None));
    let watchdog_deadline = Instant::now() + Duration::from_secs(10);
    let mut jobs = Vec::new();
    for index in 0..spec.workers {
        let storage = storage.clone();
        let maintenance = maintenance.clone();
        let cancelled_for_job = cancelled.clone();
        let pair = pairs[index / 2].clone();
        let continuation = continuations[index / 2].clone();
        let start = start.clone();
        let measurement_started = measurement_started.clone();
        let progress = Arc::new(AtomicU64::new(0));
        let progress_for_job = progress.clone();
        let seed = spec.seed;
        let workers = spec.workers;
        jobs.push(WorkerJob::injected(
            format!("worker-{index}"),
            progress,
            move || {
                let _cleanup = clear_current_thread_transaction_on_drop_for_test();
                let mut worker = WorkerContext::new(&storage, seed, index)?;
                if start.wait_for_leader(watchdog_deadline)? {
                    *measurement_started.lock().map_err(|_| {
                        crate::format_err!("benchmark measurement-start lock is poisoned")
                    })? = Some(Instant::now());
                }
                start.wait(watchdog_deadline)?;
                let mut report = if workers == 1 {
                    run_write_skew_control_phase(
                        &pair,
                        &mut worker,
                        &maintenance,
                        &cancelled_for_job,
                        &progress_for_job,
                        PhaseLimit::Schedules(schedules),
                        watchdog_deadline,
                    )?
                } else {
                    run_write_skew_phase(
                        &pair,
                        &mut worker,
                        &maintenance,
                        &cancelled_for_job,
                        &progress_for_job,
                        PhaseLimit::Schedules(schedules),
                        &continuation,
                        watchdog_deadline,
                    )?
                };
                report.index = index;
                ensure!(
                    worker.state.active_transaction().is_none(),
                    "worker {index} leaked an active transaction"
                );
                Ok(report)
            },
        ));
    }
    let reports = supervise_jobs(
        spec,
        "fixed-write-skew",
        jobs,
        watchdog_deadline,
        cancelled,
        &rendezvous_list(&pairs, Some(start)),
    )?;
    maintenance.final_barrier()?;
    let effective_elapsed = measurement_started
        .lock()
        .map_err(|_| crate::format_err!("benchmark measurement-start lock is poisoned"))?
        .context("benchmark measurement never started")?
        .elapsed();
    finish_cell(
        spec,
        &storage,
        &workload,
        &maintenance,
        &baseline,
        version_start,
        reports,
        effective_elapsed,
        effective_elapsed,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_cell(
    spec: &CellSpec,
    storage: &BenchmarkStorage,
    workload: &WorkloadState,
    maintenance: &MaintenanceGate,
    baseline: &MeasurementBaseline,
    version_start: VersionCounterSnapshot,
    mut reports: Vec<WorkerReport>,
    requested_elapsed: Duration,
    effective_elapsed: Duration,
) -> Result<FixedWriteSkewOutcome> {
    reports.sort_by_key(|report| report.index);
    ensure!(
        reports
            .iter()
            .enumerate()
            .all(|(index, report)| report.index == index),
        "benchmark worker reports have missing or duplicate identities"
    );
    let worker_metrics = reports
        .iter()
        .map(|report| report.metrics.clone())
        .collect::<Vec<_>>();
    let mut merged = WorkerMetrics::default();
    let mut combined = WorkerReport::default();
    for report in reports {
        merged.merge(&report.metrics)?;
        merge_report(&mut combined, report)?;
    }
    {
        let _attempt_guard = maintenance.attempt_guard()?;
        workload.validate(storage, baseline, merged.counts())?;
        if spec.workload == WorkloadKind::DisjointWrites {
            let worker_commits = worker_metrics
                .iter()
                .map(|metrics| metrics.counts().committed_operations)
                .collect::<Vec<_>>();
            workload.validate_disjoint_worker_commits(storage, baseline, &worker_commits)?;
        }
    }
    let restore = if spec.workload == WorkloadKind::WriteSkew {
        Some(combined.restore.restore_metrics()?)
    } else {
        None
    };
    if let Some(restore) = &restore {
        ensure!(
            merged.counts().committed_operations
                == combined
                    .schedules
                    .checked_mul(2)
                    .context("write-skew logical-operation count overflow")?,
            "write-skew schedules did not commit two logical operations each"
        );
        ensure!(
            restore.counts.committed_operations == combined.schedules,
            "write-skew schedules did not commit one restore each"
        );
    }
    storage.validate_lifecycle()?;
    let version_end = storage.version_counter_snapshot()?;
    let (gc, barrier_pruned) = maintenance.metrics()?;
    ensure!(
        matches!(
            gc.policy_mode.as_str(),
            "current-state-only" | "mvcc-compliant"
        ),
        "benchmark reported unknown GC policy mode {}",
        gc.policy_mode
    );
    let versions = version_metrics_between(version_start, version_end, barrier_pruned)?;
    let anomalous_conflict = workload.anomalous_conflict(merged.counts());
    let metrics = CellMetrics::from_workers(
        &worker_metrics,
        CellAggregateInput {
            requested_elapsed,
            effective_elapsed,
            gc,
            versions,
            schedules: (spec.workload == WorkloadKind::WriteSkew).then_some(combined.schedules),
            restore,
            anomalous_conflict,
        },
    )?;
    metrics.validate_finite()?;
    Ok(FixedWriteSkewOutcome {
        record: CellRecord {
            schema_version: SCHEMA_VERSION,
            spec: spec.clone(),
            metrics,
        },
        schedules: combined.schedules,
        observed_pairs: combined.observed_pairs,
        final_pairs: combined.final_pairs,
    })
}

fn version_metrics_between(
    start: VersionCounterSnapshot,
    end: VersionCounterSnapshot,
    barrier_pruned: u64,
) -> Result<VersionMetrics> {
    Ok(VersionMetrics {
        created: end
            .created
            .checked_sub(start.created)
            .context("benchmark MVCC created-version counter regressed")?,
        opportunistically_pruned: end
            .opportunistically_pruned
            .checked_sub(start.opportunistically_pruned)
            .context("benchmark opportunistic-pruning counter regressed")?,
        barrier_pruned,
        resident_after_final_barrier: end.resident,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::benchmark::config::{
        BackendKind, BenchmarkRequest, CellSpec, WorkloadKind, compiled_policy_name,
        compiled_transaction_features,
    };
    use crate::runtime::transaction::benchmark::workload::DeterministicRng;
    use std::string::ToString;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use std::vec::Vec;

    fn request(gc_commit_interval: u64) -> BenchmarkRequest {
        BenchmarkRequest {
            schema_version: 1,
            expected_policy: compiled_policy_name().to_string(),
            warmup_ms: 5,
            measure_ms: 20,
            repetitions: 1,
            backends: vec![BackendKind::Vmemory],
            workloads: vec![WorkloadKind::WriteSkew],
            workers: vec![2],
            seed: 0x5eed,
            gc_commit_interval,
            include_tfunc_control: false,
        }
    }

    fn spec(workload: WorkloadKind, workers: usize) -> CellSpec {
        CellSpec {
            policy: compiled_policy_name().to_string(),
            compiled_features: compiled_transaction_features()
                .iter()
                .map(|feature| (*feature).to_string())
                .collect(),
            backend: BackendKind::Vmemory,
            workload,
            workers,
            repetition: 0,
            seed: 0x5eed,
        }
    }

    #[test]
    fn retry_backoff_is_bounded_and_resets() {
        let mut rng = DeterministicRng::new(5, 0);
        for aborts in 0..64 {
            assert!(retry_delay(&mut rng, aborts) <= Duration::from_micros(64));
        }
        assert!(retry_delay(&mut rng, 0) <= Duration::from_micros(1));
    }

    #[test]
    fn write_skew_invariant_rejects_both_cleared() {
        assert!(validate_write_skew_pair(0, 0).is_err());
        validate_write_skew_pair(0, 1).unwrap();
        validate_write_skew_pair(1, 0).unwrap();
    }

    #[test]
    fn version_counter_deltas_are_checked_and_keep_final_residency() {
        let start = VersionCounterSnapshot {
            created: 10,
            opportunistically_pruned: 3,
            resident: 0,
        };
        let end = VersionCounterSnapshot {
            created: 17,
            opportunistically_pruned: 5,
            resident: 1,
        };
        let metrics = version_metrics_between(start, end, 4).unwrap();
        assert_eq!(metrics.created, 7);
        assert_eq!(metrics.opportunistically_pruned, 2);
        assert_eq!(metrics.barrier_pruned, 4);
        assert_eq!(metrics.resident_after_final_barrier, 1);
        assert!(version_metrics_between(end, start, 0).is_err());
    }

    #[test]
    fn write_skew_two_workers_preserves_invariant() {
        let outcome =
            run_fixed_write_skew_cell(&request(1), &spec(WorkloadKind::WriteSkew, 2), 20).unwrap();

        assert_eq!(outcome.record.metrics.counts.committed_operations, 40);
        assert!(outcome.record.metrics.counts.conflict_aborts >= 1);
        assert_eq!(outcome.schedules, 20);
        assert!(outcome.observed_pairs.iter().all(|pair| *pair != [0, 0]));
        assert!(outcome.final_pairs.iter().all(|pair| *pair == [1, 1]));
        let restore = outcome.record.metrics.restore.as_ref().unwrap();
        assert_eq!(restore.counts.committed_operations, 20);
        assert!(outcome.record.metrics.gc.barriers >= 21);
    }

    #[test]
    fn write_skew_one_worker_is_sequential_control() {
        let outcome =
            run_fixed_write_skew_cell(&request(2), &spec(WorkloadKind::WriteSkew, 1), 20).unwrap();

        assert_eq!(outcome.record.metrics.counts.committed_operations, 40);
        assert_eq!(outcome.record.metrics.counts.conflict_aborts, 0);
        assert_eq!(outcome.schedules, 20);
        assert!(outcome.observed_pairs.iter().all(|pair| *pair != [0, 0]));
        assert!(outcome.final_pairs.iter().all(|pair| *pair == [1, 1]));
        assert_eq!(
            outcome
                .record
                .metrics
                .restore
                .as_ref()
                .unwrap()
                .counts
                .conflict_aborts,
            0
        );
    }

    #[test]
    fn watchdog_names_cell_phase_and_last_progress() {
        let progress = Arc::new(AtomicU64::new(7));
        let job = WorkerJob::injected("parked-worker", progress, || {
            std::thread::park_timeout(Duration::from_millis(250));
            Ok(WorkerReport::default())
        });

        let error = supervise_test_jobs(
            &spec(WorkloadKind::ReadOnly, 1),
            "measurement",
            vec![job],
            Duration::from_millis(20),
            Duration::from_millis(30),
            Arc::new(AtomicBool::new(false)),
            Vec::new(),
        )
        .unwrap_err();
        let error = format!("{error:#}");

        assert!(error.contains("measurement"), "{error}");
        assert!(error.contains("read-only"), "{error}");
        assert!(error.contains("parked-worker=7"), "{error}");
    }

    #[test]
    fn paired_worker_panic_cancels_rendezvous_peer() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let rendezvous = Arc::new(CancellableRendezvous::new(2, cancelled.clone()));
        let peer_error = Arc::new(Mutex::new(None));
        let peer_exited = Arc::new(AtomicBool::new(false));

        let waiting = {
            let rendezvous = rendezvous.clone();
            let peer_error = peer_error.clone();
            let peer_exited = peer_exited.clone();
            WorkerJob::injected("waiting-peer", Arc::new(AtomicU64::new(0)), move || {
                let result = rendezvous.wait(Instant::now() + Duration::from_secs(1));
                *peer_error.lock().unwrap() = result.as_ref().err().map(ToString::to_string);
                peer_exited.store(true, Ordering::Release);
                result?;
                Ok(WorkerReport::default())
            })
        };
        let panicking = WorkerJob::injected(
            "panicking-peer",
            Arc::new(AtomicU64::new(0)),
            || -> crate::Result<WorkerReport> { panic!("injected paired-worker panic") },
        );

        let error = supervise_test_jobs(
            &spec(WorkloadKind::WriteSkew, 2),
            "measurement",
            vec![waiting, panicking],
            Duration::from_secs(1),
            Duration::from_millis(100),
            cancelled,
            vec![rendezvous],
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("injected paired-worker panic"));
        assert!(peer_exited.load(Ordering::Acquire));
        assert!(
            peer_error
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| error.contains("cancel"))
        );
    }

    #[test]
    fn pre_phase_worker_failure_cancels_waiting_peer() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let phases = Arc::new(CellPhases::new(2, cancelled.clone()));
        let peer_error = Arc::new(Mutex::new(None));
        let peer_exited = Arc::new(AtomicBool::new(false));

        let waiting = {
            let phases = phases.clone();
            let peer_error = peer_error.clone();
            let peer_exited = peer_exited.clone();
            WorkerJob::injected(
                "phase-waiting-peer",
                Arc::new(AtomicU64::new(0)),
                move || {
                    let result = phases.begin_warmup(
                        Duration::from_millis(10),
                        Instant::now() + Duration::from_secs(1),
                    );
                    *peer_error.lock().unwrap() = result.as_ref().err().map(ToString::to_string);
                    peer_exited.store(true, Ordering::Release);
                    result?;
                    Ok(WorkerReport::default())
                },
            )
        };
        let panicking = WorkerJob::injected(
            "pre-phase-panicking-peer",
            Arc::new(AtomicU64::new(0)),
            || -> crate::Result<WorkerReport> { panic!("injected pre-phase panic") },
        );

        let started = Instant::now();
        let error = supervise_test_jobs(
            &spec(WorkloadKind::ReadOnly, 2),
            "warmup-and-measurement",
            vec![waiting, panicking],
            Duration::from_secs(1),
            Duration::from_millis(100),
            cancelled,
            vec![phases.rendezvous()],
        )
        .unwrap_err();

        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(format!("{error:#}").contains("injected pre-phase panic"));
        assert!(peer_exited.load(Ordering::Acquire));
        assert!(
            peer_error
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|error| error.contains("cancel"))
        );
    }

    #[test]
    fn short_parallel_cell_runs_and_validates() {
        let record = run_cell(&request(4), &spec(WorkloadKind::DisjointWrites, 2)).unwrap();
        assert!(record.metrics.counts.committed_operations > 0);
        assert!(!record.metrics.anomalous_conflict);
    }

    #[test]
    fn short_parallel_non_skew_cell_supports_odd_worker_count() {
        let record = run_cell(&request(4), &spec(WorkloadKind::DisjointWrites, 3)).unwrap();
        assert!(record.metrics.counts.committed_operations > 0);
        assert!(!record.metrics.anomalous_conflict);
    }
}
