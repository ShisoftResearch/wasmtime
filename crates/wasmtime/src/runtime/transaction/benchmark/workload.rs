use super::config::{CellSpec, WorkloadKind};
use super::metrics::AttemptCounts;
use super::storage::{AttemptOutcome, BenchmarkStorage};
use crate::prelude::*;
use crate::runtime::transaction::TransactionState;
use std::sync::Arc;
use std::time::Instant;
use std::vec::Vec;

const PAGE_BYTES: u64 = 64 * 1024;
const GRANULE_BYTES: u64 = 64;
const READ_ONLY_START: u64 = 0;
const READ_ONLY_GRANULES: u64 = 256;
const HOT_KEY_GRANULE: u64 = 256;
const DISJOINT_START: u64 = 257;
const DISJOINT_COUNTERS: usize = 16;
const WRITE_SKEW_START: u64 = 288;
const WRITE_SKEW_GRANULES: u64 = 16;
const READS_PER_ATTEMPT: u64 = 8;
const WORKER_SEED_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;
const XORSHIFT64_STAR_MULTIPLIER: u64 = 0x2545_f491_4f6c_dd1d;

pub(super) struct WorkloadState {
    storage: Arc<BenchmarkStorage>,
    kind: WorkloadKind,
    seed: u64,
    workers: usize,
}

pub(super) struct WorkerContext {
    pub(super) index: usize,
    pub(super) storage: Arc<BenchmarkStorage>,
    pub(super) state: TransactionState,
    pub(super) rng: DeterministicRng,
    pub(super) sequence: u64,
    pub(super) write_skew_role: usize,
    pub(super) write_skew_initial_attempt: bool,
    pub(super) rendezvous_deadline: Option<Instant>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct OperationEffect {
    pub(super) checksum: u64,
    pub(super) writes: u64,
    pub(super) counter_delta: u64,
}

#[derive(Clone, Debug)]
pub(super) struct MeasurementBaseline {
    disjoint_counters: Vec<u64>,
    hot_key_counter: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub(super) fn new(run_seed: u64, worker_index: usize) -> Self {
        let derived = run_seed ^ (worker_index as u64).wrapping_mul(WORKER_SEED_MULTIPLIER);
        Self {
            state: if derived == 0 {
                WORKER_SEED_MULTIPLIER
            } else {
                derived
            },
        }
    }

    pub(super) fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(XORSHIFT64_STAR_MULTIPLIER)
    }
}

impl WorkerContext {
    pub(super) fn new(
        storage: &Arc<BenchmarkStorage>,
        run_seed: u64,
        index: usize,
    ) -> Result<Self> {
        Ok(Self {
            index,
            storage: storage.clone(),
            state: storage.new_transaction_state()?,
            rng: DeterministicRng::new(run_seed, index),
            sequence: 0,
            write_skew_role: index % 2,
            write_skew_initial_attempt: false,
            rendezvous_deadline: None,
        })
    }

    pub(super) fn begin_write_skew_operation(&mut self, role: usize, deadline: Instant) {
        self.write_skew_role = role;
        self.write_skew_initial_attempt = true;
        self.rendezvous_deadline = Some(deadline);
    }
}

pub(super) fn write_skew_addresses(pair: usize) -> Result<[u64; 2]> {
    let first = WRITE_SKEW_START
        .checked_add(
            u64::try_from(pair)
                .context("write-skew pair index does not fit u64")?
                .checked_mul(2)
                .context("write-skew pair granule overflow")?,
        )
        .context("write-skew pair granule overflow")?;
    ensure!(
        first + 1 < WRITE_SKEW_START + WRITE_SKEW_GRANULES,
        "write-skew pair {pair} exceeds the initialized dataset"
    );
    Ok([granule_addr(first), granule_addr(first + 1)])
}

pub(super) fn read_committed_u64s(
    storage: &BenchmarkStorage,
    addresses: [u64; 2],
) -> Result<[u64; 2]> {
    let values = read_values(storage, addresses)?;
    values
        .try_into()
        .map_err(|_| crate::format_err!("write-skew pair read returned the wrong value count"))
}

impl WorkloadState {
    pub(super) fn initialize(storage: &Arc<BenchmarkStorage>, spec: &CellSpec) -> Result<Self> {
        ensure!(
            spec.workers <= DISJOINT_COUNTERS,
            "benchmark supports at most {DISJOINT_COUNTERS} workers"
        );
        ensure!(
            WRITE_SKEW_START
                .checked_add(WRITE_SKEW_GRANULES)
                .and_then(|granules| granules.checked_mul(GRANULE_BYTES))
                .is_some_and(|bytes| bytes <= PAGE_BYTES),
            "benchmark dataset must fit in one 64 KiB tmemory page"
        );

        let mut state = storage.new_transaction_state()?;
        storage.begin(&mut state)?;
        let initialized = (|| {
            for granule in READ_ONLY_START..READ_ONLY_START + READ_ONLY_GRANULES {
                storage.stage_u64(
                    &mut state,
                    granule_addr(granule),
                    read_only_value(spec.seed, granule),
                )?;
            }
            storage.stage_u64(&mut state, hot_key_addr(), 0)?;
            for worker in 0..DISJOINT_COUNTERS {
                storage.stage_u64(&mut state, disjoint_addr(worker), 0)?;
            }
            for granule in WRITE_SKEW_START..WRITE_SKEW_START + WRITE_SKEW_GRANULES {
                storage.stage_u64(&mut state, granule_addr(granule), 1)?;
            }
            Ok(())
        })();
        match storage.finish_attempt(&mut state, initialized)? {
            AttemptOutcome::Committed(()) => Ok(Self {
                storage: storage.clone(),
                kind: spec.workload,
                seed: spec.seed,
                workers: spec.workers,
            }),
            AttemptOutcome::Conflict => bail!("benchmark dataset initialization conflicted"),
        }
    }

    pub(super) fn execute_attempt(
        &self,
        worker: &mut WorkerContext,
        sequence: u64,
    ) -> Result<AttemptOutcome<OperationEffect>> {
        ensure!(
            worker.index < self.workers,
            "worker {} is outside the benchmark worker set",
            worker.index
        );
        self.storage.begin(&mut worker.state)?;
        worker.sequence = sequence;
        let result = match self.kind {
            WorkloadKind::ReadOnly => read_eight_and_checksum(&self.storage, worker),
            WorkloadKind::DisjointWrites => {
                increment(&self.storage, worker, disjoint_addr(worker.index))
            }
            WorkloadKind::HotKey => increment(&self.storage, worker, hot_key_addr()),
            WorkloadKind::WriteSkew => unreachable!("implemented by the paired schedule"),
        };
        self.storage.finish_attempt(&mut worker.state, result)
    }

    pub(super) fn capture_baseline(
        &self,
        storage: &BenchmarkStorage,
    ) -> Result<MeasurementBaseline> {
        Ok(MeasurementBaseline {
            disjoint_counters: read_values(storage, (0..DISJOINT_COUNTERS).map(disjoint_addr))?,
            hot_key_counter: read_current_u64(storage, hot_key_addr())?,
        })
    }

    pub(super) fn validate(
        &self,
        storage: &BenchmarkStorage,
        baseline: &MeasurementBaseline,
        counts: &AttemptCounts,
    ) -> Result<()> {
        counts.validate()?;
        match self.kind {
            WorkloadKind::ReadOnly => {
                let actual = read_values(
                    storage,
                    (READ_ONLY_START..READ_ONLY_START + READ_ONLY_GRANULES).map(granule_addr),
                )?;
                let expected = (READ_ONLY_START..READ_ONLY_START + READ_ONLY_GRANULES)
                    .map(|granule| read_only_value(self.seed, granule))
                    .collect::<Vec<_>>();
                validate_disjoint_counters(&expected, &actual)
                    .context("read-only dataset changed during benchmark")
            }
            WorkloadKind::DisjointWrites => {
                let actual = read_values(storage, (0..DISJOINT_COUNTERS).map(disjoint_addr))?;
                ensure!(
                    actual.len() == baseline.disjoint_counters.len(),
                    "disjoint counter baseline length changed"
                );
                let measured_delta = actual.iter().zip(&baseline.disjoint_counters).try_fold(
                    0u64,
                    |total, (actual, baseline)| {
                        let delta = actual
                            .checked_sub(*baseline)
                            .context("disjoint counter regressed below its post-warmup baseline")?;
                        total
                            .checked_add(delta)
                            .context("disjoint counter delta overflow")
                    },
                )?;
                ensure!(
                    measured_delta == counts.committed_operations,
                    "disjoint counter delta {measured_delta} does not equal measured commits {}",
                    counts.committed_operations
                );
                Ok(())
            }
            WorkloadKind::HotKey => validate_counter_delta(
                baseline.hot_key_counter,
                read_current_u64(storage, hot_key_addr())?,
                counts.committed_operations,
            ),
            WorkloadKind::WriteSkew => Ok(()),
        }
    }

    pub(super) fn validate_disjoint_worker_commits(
        &self,
        storage: &BenchmarkStorage,
        baseline: &MeasurementBaseline,
        worker_commits: &[u64],
    ) -> Result<()> {
        ensure!(
            self.kind == WorkloadKind::DisjointWrites,
            "per-worker disjoint validation requires the disjoint-write workload"
        );
        ensure!(
            worker_commits.len() == self.workers,
            "per-worker commit count length {} does not equal worker count {}",
            worker_commits.len(),
            self.workers
        );
        let mut expected = baseline.disjoint_counters.clone();
        ensure!(
            expected.len() == DISJOINT_COUNTERS,
            "disjoint counter baseline length changed"
        );
        for (expected, commits) in expected.iter_mut().zip(worker_commits) {
            *expected = expected
                .checked_add(*commits)
                .context("expected disjoint counter value overflow")?;
        }
        let actual = read_values(storage, (0..DISJOINT_COUNTERS).map(disjoint_addr))?;
        validate_disjoint_counters(&expected, &actual)
            .context("disjoint worker counter did not match its measured commits")
    }

    pub(super) fn anomalous_conflict(&self, counts: &AttemptCounts) -> bool {
        self.kind == WorkloadKind::DisjointWrites && counts.conflict_aborts != 0
    }
}

fn granule_addr(granule: u64) -> u64 {
    granule * GRANULE_BYTES
}

fn hot_key_addr() -> u64 {
    granule_addr(HOT_KEY_GRANULE)
}

fn disjoint_addr(worker: usize) -> u64 {
    granule_addr(DISJOINT_START + worker as u64)
}

fn read_only_granule(rng: &mut DeterministicRng) -> u64 {
    READ_ONLY_START + rng.next_u64() % READ_ONLY_GRANULES
}

fn read_only_value(seed: u64, granule: u64) -> u64 {
    seed.rotate_left((granule % 64) as u32) ^ granule
}

fn read_eight_checksum(seed: u64, rng: &mut DeterministicRng) -> u64 {
    (0..READS_PER_ATTEMPT).fold(0, |checksum, _| {
        checksum.wrapping_add(read_only_value(seed, read_only_granule(rng)))
    })
}

fn read_eight_and_checksum(
    storage: &BenchmarkStorage,
    worker: &mut WorkerContext,
) -> Result<OperationEffect> {
    let mut checksum: u64 = 0;
    for _ in 0..READS_PER_ATTEMPT {
        checksum = checksum.wrapping_add(storage.read_u64(
            &mut worker.state,
            granule_addr(read_only_granule(&mut worker.rng)),
        )?);
    }
    Ok(OperationEffect {
        checksum,
        writes: 0,
        counter_delta: 0,
    })
}

fn increment(
    storage: &BenchmarkStorage,
    worker: &mut WorkerContext,
    addr: u64,
) -> Result<OperationEffect> {
    let value = storage.read_u64(&mut worker.state, addr)?;
    let value = value.checked_add(1).context("benchmark counter overflow")?;
    storage.stage_u64(&mut worker.state, addr, value)?;
    Ok(OperationEffect {
        checksum: 0,
        writes: 1,
        counter_delta: 1,
    })
}

fn read_values(
    storage: &BenchmarkStorage,
    addresses: impl IntoIterator<Item = u64>,
) -> Result<Vec<u64>> {
    let mut state = storage.new_transaction_state()?;
    storage.begin(&mut state)?;
    let result = addresses
        .into_iter()
        .map(|addr| storage.read_u64(&mut state, addr))
        .collect::<Result<Vec<_>>>();
    match storage.finish_attempt(&mut state, result)? {
        AttemptOutcome::Committed(values) => Ok(values),
        AttemptOutcome::Conflict => bail!("benchmark committed-state read conflicted"),
    }
}

fn read_current_u64(storage: &BenchmarkStorage, addr: u64) -> Result<u64> {
    let mut state = storage.new_transaction_state()?;
    storage.begin(&mut state)?;
    let result = storage.read_u64(&mut state, addr);
    match storage.finish_attempt(&mut state, result)? {
        AttemptOutcome::Committed(value) => Ok(value),
        AttemptOutcome::Conflict => bail!("benchmark committed-state read conflicted"),
    }
}

fn validate_disjoint_counters(expected: &[u64], actual: &[u64]) -> Result<()> {
    ensure!(
        expected == actual,
        "counter values differ from their expected values"
    );
    Ok(())
}

fn validate_counter_delta(baseline: u64, actual: u64, expected_delta: u64) -> Result<()> {
    let expected = baseline
        .checked_add(expected_delta)
        .context("expected counter value overflow")?;
    ensure!(
        actual == expected,
        "counter value {actual} does not equal expected value {expected}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::config::BackendKind;
    use super::*;
    use crate::runtime::transaction::clear_current_thread_transaction_on_drop_for_test;

    fn spec(workload: WorkloadKind, workers: usize) -> CellSpec {
        CellSpec {
            policy: "test".to_string(),
            compiled_features: Vec::new(),
            backend: BackendKind::Vmemory,
            workload,
            workers,
            repetition: 0,
            seed: 7,
        }
    }

    #[test]
    fn deterministic_rng_repeats_for_the_same_worker_seed() {
        let first = (0..16)
            .scan(DeterministicRng::new(7, 3), |rng, _| Some(rng.next_u64()))
            .collect::<Vec<_>>();
        let second = (0..16)
            .scan(DeterministicRng::new(7, 3), |rng, _| Some(rng.next_u64()))
            .collect::<Vec<_>>();
        assert_eq!(first, second);
    }

    #[test]
    fn disjoint_write_validation_rejects_a_missing_increment() {
        let expected = vec![11, 9, 7];
        let actual = vec![11, 8, 7];
        assert!(validate_disjoint_counters(&expected, &actual).is_err());
    }

    #[test]
    fn hot_key_validation_rejects_a_lost_update() {
        assert!(validate_counter_delta(100, 113, 14).is_err());
    }

    #[test]
    fn read_only_checksum_covers_eight_deterministic_granules() {
        let seed = 7;
        let mut rng = DeterministicRng::new(seed, 3);
        let expected = (0..READS_PER_ATTEMPT)
            .map(|_| read_only_value(seed, read_only_granule(&mut rng)))
            .fold(0, u64::wrapping_add);

        let mut rng = DeterministicRng::new(seed, 3);
        assert_eq!(read_eight_checksum(seed, &mut rng), expected);
    }

    #[test]
    fn read_only_attempt_reads_initialized_granules_and_validates_them() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let workload =
            WorkloadState::initialize(&storage, &spec(WorkloadKind::ReadOnly, 1)).unwrap();
        let mut worker = WorkerContext::new(&storage, 7, 0).unwrap();
        let mut expected_rng = DeterministicRng::new(7, 0);

        let outcome = workload.execute_attempt(&mut worker, 0).unwrap();
        let AttemptOutcome::Committed(effect) = outcome else {
            panic!("read-only attempt unexpectedly conflicted");
        };
        assert_eq!(effect.checksum, read_eight_checksum(7, &mut expected_rng));
        assert_eq!(effect.writes, 0);
        assert_eq!(effect.counter_delta, 0);

        let baseline = workload.capture_baseline(&storage).unwrap();
        workload
            .validate(&storage, &baseline, &AttemptCounts::default())
            .unwrap();
    }

    #[test]
    fn counter_workloads_validate_measured_deltas_from_the_warmup_baseline() {
        let _cleanup = clear_current_thread_transaction_on_drop_for_test();
        let storage = BenchmarkStorage::new(BackendKind::Vmemory).unwrap();
        let disjoint =
            WorkloadState::initialize(&storage, &spec(WorkloadKind::DisjointWrites, 2)).unwrap();
        let mut first = WorkerContext::new(&storage, 7, 0).unwrap();
        let mut second = WorkerContext::new(&storage, 7, 1).unwrap();

        assert!(matches!(
            disjoint.execute_attempt(&mut first, 0).unwrap(),
            AttemptOutcome::Committed(OperationEffect {
                writes: 1,
                counter_delta: 1,
                ..
            })
        ));
        let baseline = disjoint.capture_baseline(&storage).unwrap();
        assert!(matches!(
            disjoint.execute_attempt(&mut first, 1).unwrap(),
            AttemptOutcome::Committed(_)
        ));
        assert!(matches!(
            disjoint.execute_attempt(&mut second, 0).unwrap(),
            AttemptOutcome::Committed(_)
        ));
        let counts = AttemptCounts {
            attempts: 2,
            successful_attempts: 2,
            committed_operations: 2,
            ..AttemptCounts::default()
        };
        disjoint.validate(&storage, &baseline, &counts).unwrap();
        disjoint
            .validate_disjoint_worker_commits(&storage, &baseline, &[1, 1])
            .unwrap();
        assert!(!disjoint.anomalous_conflict(&counts));

        let hot = WorkloadState::initialize(&storage, &spec(WorkloadKind::HotKey, 1)).unwrap();
        let mut worker = WorkerContext::new(&storage, 7, 0).unwrap();
        assert!(matches!(
            hot.execute_attempt(&mut worker, 0).unwrap(),
            AttemptOutcome::Committed(_)
        ));
        let baseline = hot.capture_baseline(&storage).unwrap();
        assert!(matches!(
            hot.execute_attempt(&mut worker, 1).unwrap(),
            AttemptOutcome::Committed(_)
        ));
        let counts = AttemptCounts {
            attempts: 1,
            successful_attempts: 1,
            committed_operations: 1,
            ..AttemptCounts::default()
        };
        hot.validate(&storage, &baseline, &counts).unwrap();
    }
}
