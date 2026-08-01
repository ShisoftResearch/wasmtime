use super::super::ConcurrencyControl;
use crate::prelude::*;
use serde_derive::{Deserialize, Serialize};
use std::path::Path;

pub(super) const MAX_BENCHMARK_PHASE_MS: u64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum BackendKind {
    Vmemory,
    FileBacked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum WorkloadKind {
    ReadOnly,
    DisjointWrites,
    HotKey,
    WriteSkew,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct BenchmarkRequest {
    pub schema_version: u32,
    pub expected_policy: String,
    pub warmup_ms: u64,
    pub measure_ms: u64,
    pub repetitions: u32,
    pub backends: Vec<BackendKind>,
    pub workloads: Vec<WorkloadKind>,
    pub workers: Vec<usize>,
    pub seed: u64,
    pub gc_commit_interval: u64,
    pub include_tfunc_control: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CellSpec {
    pub policy: String,
    pub visibility_mode: String,
    pub concurrency_control: String,
    pub compiled_features: Vec<String>,
    pub backend: BackendKind,
    pub workload: WorkloadKind,
    pub workers: usize,
    pub repetition: u32,
    pub seed: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum MatrixDisposition {
    Run,
    Skipped(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct MatrixEntry {
    pub spec: CellSpec,
    pub disposition: MatrixDisposition,
}

impl BenchmarkRequest {
    pub(super) fn quick(expected_policy: String) -> Self {
        Self {
            schema_version: 2,
            expected_policy,
            warmup_ms: 100,
            measure_ms: 500,
            repetitions: 1,
            backends: vec![BackendKind::Vmemory, BackendKind::FileBacked],
            workloads: vec![
                WorkloadKind::ReadOnly,
                WorkloadKind::DisjointWrites,
                WorkloadKind::HotKey,
                WorkloadKind::WriteSkew,
            ],
            workers: vec![1, 2, 4, 8, 16],
            seed: 0x5eed_5eed_d15c_a11e,
            gc_commit_interval: 4096,
            include_tfunc_control: true,
        }
    }

    pub(super) fn read(path: &Path) -> Result<Self> {
        let contents = std::fs::read(path)?;
        Ok(serde_json::from_slice(&contents)?)
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 2,
            "unsupported request schema version {}",
            self.schema_version
        );
        ensure!(self.warmup_ms > 0, "warmup duration must be nonzero");
        ensure!(self.measure_ms > 0, "measurement duration must be nonzero");
        ensure!(
            self.warmup_ms <= MAX_BENCHMARK_PHASE_MS && self.measure_ms <= MAX_BENCHMARK_PHASE_MS,
            "benchmark warmup and measurement duration must each be at most {MAX_BENCHMARK_PHASE_MS} milliseconds"
        );
        ensure!(self.repetitions > 0, "repetitions must be nonzero");
        ensure!(
            self.gc_commit_interval > 0,
            "GC commit interval must be nonzero"
        );
        ensure!(!self.backends.is_empty(), "backend axis must not be empty");
        ensure!(
            !self.workloads.is_empty(),
            "workload axis must not be empty"
        );
        ensure!(!self.workers.is_empty(), "worker axis must not be empty");
        ensure!(
            !has_duplicates(&self.backends),
            "backend axis must not contain duplicates"
        );
        ensure!(
            !has_duplicates(&self.workloads),
            "workload axis must not contain duplicates"
        );
        ensure!(
            !has_duplicates(&self.workers),
            "worker axis must not contain duplicates"
        );
        ensure!(
            self.workers.iter().all(|workers| *workers > 0),
            "worker counts must be nonzero"
        );
        ensure!(
            self.expected_policy == compiled_policy_name(),
            "requested policy {} does not match compiled policy {}",
            self.expected_policy,
            compiled_policy_name()
        );
        Ok(())
    }

    pub(super) fn cells(&self, available_parallelism: usize) -> Vec<MatrixEntry> {
        let features = compiled_transaction_features()
            .iter()
            .map(|feature| (*feature).to_string())
            .collect::<Vec<_>>();
        let mut cells = Vec::new();

        for &backend in &self.backends {
            for &workload in &self.workloads {
                for &workers in &self.workers {
                    for repetition in 0..self.repetitions {
                        let spec = CellSpec {
                            policy: compiled_policy_name().to_string(),
                            visibility_mode: compiled_visibility_mode().to_string(),
                            concurrency_control: compiled_concurrency_control_name().to_string(),
                            compiled_features: features.clone(),
                            backend,
                            workload,
                            workers,
                            repetition,
                            seed: self.seed,
                        };
                        let disposition = if workers > available_parallelism {
                            MatrixDisposition::Skipped(format!(
                                "worker count {workers} exceeds available parallelism {available_parallelism}"
                            ))
                        } else {
                            MatrixDisposition::Run
                        };
                        cells.push(MatrixEntry { spec, disposition });
                    }
                }
            }
        }

        cells
    }
}

fn has_duplicates<T: Eq>(items: &[T]) -> bool {
    items
        .iter()
        .enumerate()
        .any(|(index, item)| items[index + 1..].contains(item))
}

pub(super) fn compiled_policy_name() -> &'static str {
    match (
        cfg!(feature = "transaction-mvcc"),
        ConcurrencyControl::default_for_build(),
    ) {
        (false, policy) => single_version_policy_name(policy),
        (true, ConcurrencyControl::LockBased) => "mvcc-lockbased",
        (true, ConcurrencyControl::NoWaitAbort) => "mvcc-no-wait",
        (true, ConcurrencyControl::StrictTwoPhaseLocking) => "mvcc-strict-2pl",
        (true, ConcurrencyControl::WoundWait) => "mvcc-wound-wait",
        (true, ConcurrencyControl::WaitDie) => "mvcc-wait-die",
        (true, ConcurrencyControl::OptimisticValidation) => "mvcc-optimistic",
        (true, ConcurrencyControl::TimestampOrdering) => "mvcc-timestamp",
    }
}

pub(super) fn compiled_visibility_mode() -> &'static str {
    if cfg!(feature = "transaction-mvcc") {
        "mvcc"
    } else {
        "single-version"
    }
}

pub(super) fn compiled_concurrency_control_name() -> &'static str {
    single_version_policy_name(ConcurrencyControl::default_for_build())
}

fn single_version_policy_name(policy: ConcurrencyControl) -> &'static str {
    match policy {
        ConcurrencyControl::LockBased => "lockbased",
        ConcurrencyControl::NoWaitAbort => "no-wait",
        ConcurrencyControl::StrictTwoPhaseLocking => "strict-2pl",
        ConcurrencyControl::WoundWait => "wound-wait",
        ConcurrencyControl::WaitDie => "wait-die",
        ConcurrencyControl::OptimisticValidation => "optimistic",
        ConcurrencyControl::TimestampOrdering => "timestamp",
    }
}

pub(super) fn compiled_transaction_features() -> &'static [&'static str] {
    match (
        cfg!(feature = "transaction-mvcc"),
        ConcurrencyControl::default_for_build(),
    ) {
        (false, ConcurrencyControl::LockBased) => &["transaction-cc-lockbased"],
        (false, ConcurrencyControl::NoWaitAbort) => &["transaction-cc-nowait-abort"],
        (false, ConcurrencyControl::StrictTwoPhaseLocking) => &["transaction-cc-strict-2pl"],
        (false, ConcurrencyControl::WoundWait) => &["transaction-cc-wound-wait"],
        (false, ConcurrencyControl::WaitDie) => &["transaction-cc-wait-die"],
        (false, ConcurrencyControl::OptimisticValidation) => {
            &["transaction-cc-optimistic-validation"]
        }
        (false, ConcurrencyControl::TimestampOrdering) => &["transaction-cc-timestamp-ordering"],
        (true, ConcurrencyControl::LockBased) => &["transaction-mvcc", "transaction-cc-lockbased"],
        (true, ConcurrencyControl::NoWaitAbort) => {
            &["transaction-mvcc", "transaction-cc-nowait-abort"]
        }
        (true, ConcurrencyControl::StrictTwoPhaseLocking) => {
            &["transaction-mvcc", "transaction-cc-strict-2pl"]
        }
        (true, ConcurrencyControl::WoundWait) => &["transaction-mvcc", "transaction-cc-wound-wait"],
        (true, ConcurrencyControl::WaitDie) => &["transaction-mvcc", "transaction-cc-wait-die"],
        (true, ConcurrencyControl::OptimisticValidation) => {
            &["transaction-mvcc", "transaction-cc-optimistic-validation"]
        }
        (true, ConcurrencyControl::TimestampOrdering) => {
            &["transaction-mvcc", "transaction-cc-timestamp-ordering"]
        }
    }
}

#[test]
fn quick_request_expands_and_marks_unsupported_workers() {
    let request = BenchmarkRequest::quick(compiled_policy_name().to_string());
    let cells = request.cells(4);
    assert_eq!(cells.len(), 2 * 4 * 5);
    assert!(cells.iter().any(|entry| {
        entry.spec.workers == 16
            && matches!(&entry.disposition, MatrixDisposition::Skipped(reason)
                if reason == "worker count 16 exceeds available parallelism 4")
    }));
}

#[test]
fn request_rejects_a_policy_other_than_the_compiled_policy() {
    let mut request = BenchmarkRequest::quick("definitely-not-this-build".into());
    let error = request.validate().unwrap_err().to_string();
    assert!(error.contains("requested policy"), "{error}");
    request.expected_policy = compiled_policy_name().into();
    request.validate().unwrap();
}

#[test]
fn approved_single_version_policy_names_match_the_orchestrator_contract() {
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::LockBased),
        "lockbased"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::NoWaitAbort),
        "no-wait"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::StrictTwoPhaseLocking),
        "strict-2pl"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::WoundWait),
        "wound-wait"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::WaitDie),
        "wait-die"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::OptimisticValidation),
        "optimistic"
    );
    assert_eq!(
        single_version_policy_name(ConcurrencyControl::TimestampOrdering),
        "timestamp"
    );
}

#[test]
fn request_rejects_phase_durations_above_twenty_four_hours() {
    const TWENTY_FOUR_HOURS_MS: u64 = 24 * 60 * 60 * 1_000;
    let mut request = BenchmarkRequest::quick(compiled_policy_name().into());

    request.warmup_ms = TWENTY_FOUR_HOURS_MS + 1;
    let error = request.validate().unwrap_err().to_string();
    assert!(error.contains("at most"), "{error}");

    request.warmup_ms = TWENTY_FOUR_HOURS_MS;
    request.measure_ms = TWENTY_FOUR_HOURS_MS + 1;
    let error = request.validate().unwrap_err().to_string();
    assert!(error.contains("at most"), "{error}");

    request.measure_ms = TWENTY_FOUR_HOURS_MS;
    request.validate().unwrap();
}
