use crate::prelude::*;
use serde_derive::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub(super) struct AttemptCounts {
    pub attempts: u64,
    pub successful_attempts: u64,
    pub conflict_aborts: u64,
    pub committed_operations: u64,
    pub retries: u64,
    pub unexpected_errors: u64,
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
