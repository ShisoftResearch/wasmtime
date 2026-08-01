use crate::prelude::*;
use serde_derive::Serialize;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(super) struct LatencyHistogram {
    buckets: [u64; 64],
    samples: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            samples: 0,
        }
    }
}

impl LatencyHistogram {
    pub(super) fn record(&mut self, elapsed: Duration) -> Result<()> {
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX).max(1);
        let bucket = (63 - nanos.leading_zeros()) as usize;
        let bucket_count = checked_add(self.buckets[bucket], 1, "latency bucket")?;
        let samples = checked_add(self.samples, 1, "latency samples")?;
        self.buckets[bucket] = bucket_count;
        self.samples = samples;
        Ok(())
    }

    fn merge(&mut self, other: &Self) -> Result<()> {
        let mut buckets = self.buckets;
        for (bucket, other_bucket) in buckets.iter_mut().zip(other.buckets) {
            *bucket = checked_add(*bucket, other_bucket, "latency bucket")?;
        }
        let samples = checked_add(self.samples, other.samples, "latency samples")?;
        self.buckets = buckets;
        self.samples = samples;
        Ok(())
    }

    pub(super) fn percentile(&self, percentile: f64) -> Option<u64> {
        let required_samples = match percentile {
            0.50 => 2,
            0.95 => 20,
            0.99 => 100,
            _ => return None,
        };
        if self.samples < required_samples {
            return None;
        }

        let target = (self.samples as f64 * percentile).ceil() as u64;
        let mut seen = 0;
        for (bucket, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                return Some(if bucket == 63 {
                    u64::MAX
                } else {
                    (1u64 << (bucket + 1)) - 1
                });
            }
        }
        unreachable!("histogram sample count must equal the sum of its buckets")
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct AttemptCounts {
    pub attempts: u64,
    pub successful_attempts: u64,
    pub conflict_aborts: u64,
    pub committed_operations: u64,
    pub retries: u64,
    pub unexpected_errors: u64,
}

impl AttemptCounts {
    pub(super) fn validate(&self) -> Result<()> {
        if self.successful_attempts.checked_add(self.conflict_aborts) != Some(self.attempts) {
            bail!("attempts must equal successful attempts plus conflict aborts");
        }
        if self.committed_operations != self.successful_attempts {
            bail!("committed operations must equal successful attempts");
        }
        if self.attempts.checked_sub(self.committed_operations) != Some(self.retries) {
            bail!("retries must equal attempts minus committed operations");
        }
        Ok(())
    }

    fn merge(&mut self, other: &Self) -> Result<()> {
        let attempts = checked_add(self.attempts, other.attempts, "attempts")?;
        let successful_attempts = checked_add(
            self.successful_attempts,
            other.successful_attempts,
            "successful attempts",
        )?;
        let conflict_aborts = checked_add(
            self.conflict_aborts,
            other.conflict_aborts,
            "conflict aborts",
        )?;
        let committed_operations = checked_add(
            self.committed_operations,
            other.committed_operations,
            "committed operations",
        )?;
        let retries = checked_add(self.retries, other.retries, "retries")?;
        let unexpected_errors = checked_add(
            self.unexpected_errors,
            other.unexpected_errors,
            "unexpected errors",
        )?;
        *self = Self {
            attempts,
            successful_attempts,
            conflict_aborts,
            committed_operations,
            retries,
            unexpected_errors,
        };
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct WorkerMetrics {
    counts: AttemptCounts,
    latency: LatencyHistogram,
}

impl WorkerMetrics {
    pub(super) fn record_commit(&mut self, elapsed: Duration) -> Result<()> {
        let attempts = checked_add(self.counts.attempts, 1, "attempts")?;
        let successful_attempts =
            checked_add(self.counts.successful_attempts, 1, "successful attempts")?;
        let committed_operations =
            checked_add(self.counts.committed_operations, 1, "committed operations")?;
        self.latency.record(elapsed)?;
        self.counts.attempts = attempts;
        self.counts.successful_attempts = successful_attempts;
        self.counts.committed_operations = committed_operations;
        Ok(())
    }

    pub(super) fn record_conflict(&mut self) -> Result<()> {
        let attempts = checked_add(self.counts.attempts, 1, "attempts")?;
        let conflict_aborts = checked_add(self.counts.conflict_aborts, 1, "conflict aborts")?;
        let retries = checked_add(self.counts.retries, 1, "retries")?;
        self.counts.attempts = attempts;
        self.counts.conflict_aborts = conflict_aborts;
        self.counts.retries = retries;
        Ok(())
    }

    pub(super) fn counts(&self) -> &AttemptCounts {
        &self.counts
    }

    pub(super) fn restore_metrics(&self) -> Result<RestoreMetrics> {
        self.counts.validate()?;
        Ok(RestoreMetrics {
            counts: self.counts.clone(),
            p50_ns: self.latency.percentile(0.50),
            p95_ns: self.latency.percentile(0.95),
            p99_ns: self.latency.percentile(0.99),
        })
    }

    pub(super) fn merge(&mut self, other: &Self) -> Result<()> {
        let mut counts = self.counts.clone();
        counts.merge(&other.counts)?;
        let mut latency = self.latency.clone();
        latency.merge(&other.latency)?;
        self.counts = counts;
        self.latency = latency;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct GcMetrics {
    pub barriers: u64,
    pub total_ns: u64,
    pub max_pause_ns: u64,
    pub elapsed_share: f64,
    pub policy_mode: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct VersionMetrics {
    pub created: u64,
    pub opportunistically_pruned: u64,
    pub barrier_pruned: u64,
    pub resident_after_final_barrier: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct FairnessMetrics {
    pub min_max_ratio: Option<f64>,
    pub coefficient_of_variation: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct RestoreMetrics {
    pub counts: AttemptCounts,
    pub p50_ns: Option<u64>,
    pub p95_ns: Option<u64>,
    pub p99_ns: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct CellMetrics {
    pub requested_elapsed_ns: u64,
    pub effective_elapsed_ns: u64,
    pub attempts_per_second: f64,
    pub committed_operations_per_second: f64,
    pub counts: AttemptCounts,
    pub p50_ns: Option<u64>,
    pub p95_ns: Option<u64>,
    pub p99_ns: Option<u64>,
    pub fairness: FairnessMetrics,
    pub gc: GcMetrics,
    pub versions: VersionMetrics,
    pub schedules_per_second: Option<f64>,
    pub restore: Option<RestoreMetrics>,
    pub anomalous_conflict: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CellAggregateInput {
    pub requested_elapsed: Duration,
    pub effective_elapsed: Duration,
    pub gc: GcMetrics,
    pub versions: VersionMetrics,
    pub schedules: Option<u64>,
    pub restore: Option<RestoreMetrics>,
    pub anomalous_conflict: bool,
}

impl CellMetrics {
    pub(super) fn from_workers(
        workers: &[WorkerMetrics],
        mut input: CellAggregateInput,
    ) -> Result<Self> {
        if input.effective_elapsed.is_zero() {
            bail!("effective elapsed time must be nonzero");
        }

        let mut merged = WorkerMetrics::default();
        for worker in workers {
            worker.counts.validate()?;
            merged.merge(worker)?;
        }
        merged.counts.validate()?;

        let elapsed_secs = input.effective_elapsed.as_secs_f64();
        input.gc.elapsed_share = input.gc.total_ns as f64
            / u64::try_from(input.effective_elapsed.as_nanos()).unwrap_or(u64::MAX) as f64;

        let committed = workers
            .iter()
            .map(|worker| worker.counts.committed_operations)
            .collect::<Vec<_>>();
        let fairness = fairness(&committed);

        Ok(Self {
            requested_elapsed_ns: u64::try_from(input.requested_elapsed.as_nanos())
                .unwrap_or(u64::MAX),
            effective_elapsed_ns: u64::try_from(input.effective_elapsed.as_nanos())
                .unwrap_or(u64::MAX),
            attempts_per_second: merged.counts.attempts as f64 / elapsed_secs,
            committed_operations_per_second: merged.counts.committed_operations as f64
                / elapsed_secs,
            counts: merged.counts,
            p50_ns: merged.latency.percentile(0.50),
            p95_ns: merged.latency.percentile(0.95),
            p99_ns: merged.latency.percentile(0.99),
            fairness,
            gc: input.gc,
            versions: input.versions,
            schedules_per_second: input
                .schedules
                .map(|schedules| schedules as f64 / elapsed_secs),
            restore: input.restore,
            anomalous_conflict: input.anomalous_conflict,
        })
    }

    pub(super) fn validate_finite(&self) -> Result<()> {
        ensure!(
            self.attempts_per_second.is_finite(),
            "attempt throughput must be finite"
        );
        ensure!(
            self.committed_operations_per_second.is_finite(),
            "committed-operation throughput must be finite"
        );
        ensure!(self.gc.elapsed_share.is_finite(), "GC share must be finite");
        for (name, value) in [
            (
                "fairness minimum/maximum ratio",
                self.fairness.min_max_ratio,
            ),
            (
                "fairness coefficient of variation",
                self.fairness.coefficient_of_variation,
            ),
            ("write-skew schedule throughput", self.schedules_per_second),
        ] {
            if let Some(value) = value {
                ensure!(value.is_finite(), "{name} must be finite");
            }
        }
        Ok(())
    }
}

fn checked_add(left: u64, right: u64, counter: &str) -> Result<u64> {
    match left.checked_add(right) {
        Some(value) => Ok(value),
        None => bail!("{counter} counter overflow"),
    }
}

fn fairness(committed: &[u64]) -> FairnessMetrics {
    let Some(&maximum) = committed.iter().max() else {
        return FairnessMetrics::default();
    };
    if maximum == 0 {
        return FairnessMetrics::default();
    }

    let minimum = committed.iter().min().copied().unwrap_or(0);
    let mean = committed.iter().map(|count| *count as f64).sum::<f64>() / committed.len() as f64;
    let variance = committed
        .iter()
        .map(|count| (*count as f64 - mean).powi(2))
        .sum::<f64>()
        / committed.len() as f64;
    FairnessMetrics {
        min_max_ratio: Some(minimum as f64 / maximum as f64),
        coefficient_of_variation: Some(variance.sqrt() / mean),
    }
}

#[test]
fn percentile_requires_enough_samples_and_returns_nanoseconds() {
    let mut histogram = LatencyHistogram::default();
    for micros in 1..=100 {
        histogram.record(Duration::from_micros(micros)).unwrap();
    }
    assert!(histogram.percentile(0.50).unwrap() >= 50_000);
    assert!(histogram.percentile(0.95).unwrap() >= 95_000);
    assert!(histogram.percentile(0.99).unwrap() >= 99_000);

    let mut sparse = LatencyHistogram::default();
    sparse.record(Duration::from_nanos(7)).unwrap();
    assert_eq!(sparse.percentile(0.50), None);
}

#[test]
fn attempt_identity_rejects_inconsistent_counts() {
    let valid = AttemptCounts {
        attempts: 9,
        successful_attempts: 4,
        conflict_aborts: 5,
        committed_operations: 4,
        retries: 5,
        unexpected_errors: 0,
    };
    valid.validate().unwrap();
    let invalid = AttemptCounts {
        retries: 4,
        ..valid
    };
    assert!(invalid.validate().is_err());

    let overflow = AttemptCounts {
        attempts: 0,
        successful_attempts: u64::MAX,
        conflict_aborts: 1,
        committed_operations: u64::MAX,
        retries: 0,
        unexpected_errors: 0,
    };
    assert!(overflow.validate().is_err());
}

#[test]
fn aggregate_reports_fairness_and_effective_elapsed_throughput() {
    let mut workers = [
        WorkerMetrics::default(),
        WorkerMetrics::default(),
        WorkerMetrics::default(),
    ];
    for worker in &mut workers[..2] {
        for _ in 0..100 {
            worker.record_commit(Duration::from_micros(1)).unwrap();
        }
    }
    for _ in 0..50 {
        workers[2].record_commit(Duration::from_micros(1)).unwrap();
    }

    let metrics = CellMetrics::from_workers(
        &workers,
        CellAggregateInput {
            requested_elapsed: Duration::from_secs(4),
            effective_elapsed: Duration::from_secs(2),
            gc: GcMetrics::default(),
            versions: VersionMetrics::default(),
            schedules: Some(6),
            restore: None,
            anomalous_conflict: false,
        },
    )
    .unwrap();

    assert_eq!(metrics.counts.committed_operations, 250);
    assert_eq!(metrics.committed_operations_per_second, 125.0);
    assert_eq!(metrics.attempts_per_second, 125.0);
    assert_eq!(metrics.schedules_per_second, Some(3.0));
    assert_eq!(metrics.fairness.min_max_ratio, Some(0.5));
    assert!(metrics.fairness.coefficient_of_variation.unwrap() > 0.0);
}

#[test]
fn worker_recording_rejects_counter_overflow_without_wrapping() {
    let mut worker = WorkerMetrics {
        counts: AttemptCounts {
            attempts: u64::MAX,
            successful_attempts: u64::MAX,
            conflict_aborts: 0,
            committed_operations: u64::MAX,
            retries: 0,
            unexpected_errors: 0,
        },
        latency: LatencyHistogram::default(),
    };

    assert!(worker.record_commit(Duration::from_nanos(1)).is_err());
    assert_eq!(worker.counts.attempts, u64::MAX);
}

#[test]
fn aggregation_rejects_merged_counter_overflow() {
    let maximum = WorkerMetrics {
        counts: AttemptCounts {
            attempts: u64::MAX,
            successful_attempts: u64::MAX,
            conflict_aborts: 0,
            committed_operations: u64::MAX,
            retries: 0,
            unexpected_errors: 0,
        },
        latency: LatencyHistogram::default(),
    };
    let mut one = WorkerMetrics::default();
    one.record_commit(Duration::from_nanos(1)).unwrap();

    assert!(
        CellMetrics::from_workers(
            &[maximum, one],
            CellAggregateInput {
                effective_elapsed: Duration::from_secs(1),
                ..Default::default()
            },
        )
        .is_err()
    );
}
