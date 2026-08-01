use super::config::{
    BackendKind, CellSpec, WorkloadKind, compiled_concurrency_control_name, compiled_policy_name,
    compiled_transaction_features, compiled_visibility_mode,
};
use super::metrics::CellMetrics;
use super::*;
use serde_derive::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub(super) const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "record_type", rename_all = "snake_case")]
pub(super) enum BenchmarkRecord {
    Driver(DriverRecord),
    Cell(CellRecord),
    TfuncControl(TfuncControlRecord),
    SkippedCell(SkippedCellRecord),
    Failure(FailureRecord),
    Complete(CompleteRecord),
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct DriverRecord {
    pub schema_version: u32,
    pub compiled_policy: String,
    pub visibility_mode: String,
    pub concurrency_control: String,
    pub compiled_features: Vec<String>,
    pub available_parallelism: usize,
    pub gc_policy_mode: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CellRecord {
    pub schema_version: u32,
    pub spec: CellSpec,
    pub metrics: CellMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct TfuncControlRecord {
    pub schema_version: u32,
    pub policy: String,
    pub visibility_mode: String,
    pub concurrency_control: String,
    pub compiled_features: Vec<String>,
    pub backend: BackendKind,
    pub control: String,
    pub repetition: u32,
    pub seed: u64,
    pub metrics: CellMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct SkippedCellRecord {
    pub schema_version: u32,
    pub spec: CellSpec,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct FailureRecord {
    pub schema_version: u32,
    pub policy: String,
    pub cell: Option<CellSpec>,
    pub phase: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CompleteRecord {
    pub schema_version: u32,
    pub policy: String,
    pub completed_cells: usize,
    pub skipped_cells: usize,
    pub tfunc_controls: usize,
}

pub(super) struct JsonlWriter {
    writer: BufWriter<File>,
}

impl JsonlWriter {
    pub(super) fn create(path: &Path) -> Result<Self> {
        Ok(Self {
            writer: BufWriter::new(File::create(path)?),
        })
    }

    pub(super) fn write(&mut self, record: &BenchmarkRecord) -> Result<()> {
        serde_json::to_writer(&mut self.writer, record)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

#[test]
fn jsonl_round_trip_preserves_skipped_cell_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("benchmark.jsonl");
    let spec = CellSpec {
        policy: compiled_policy_name().into(),
        visibility_mode: compiled_visibility_mode().into(),
        concurrency_control: compiled_concurrency_control_name().into(),
        compiled_features: compiled_transaction_features()
            .iter()
            .map(|feature| (*feature).into())
            .collect(),
        backend: BackendKind::Vmemory,
        workload: WorkloadKind::ReadOnly,
        workers: 1,
        repetition: 0,
        seed: 42,
    };
    let record = BenchmarkRecord::SkippedCell(SkippedCellRecord {
        schema_version: SCHEMA_VERSION,
        spec: spec.clone(),
        reason: "not enough workers".into(),
    });

    let mut writer = JsonlWriter::create(&path).unwrap();
    writer.write(&record).unwrap();
    drop(writer);

    let contents = std::fs::read_to_string(path).unwrap();
    let value: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
    assert_eq!(value["record_type"], "skipped_cell");
    assert_eq!(value["schema_version"], 2);
    assert_eq!(value["spec"]["policy"], spec.policy);
    assert_eq!(value["spec"]["visibility_mode"], spec.visibility_mode);
    assert_eq!(
        value["spec"]["concurrency_control"],
        spec.concurrency_control
    );
    assert_eq!(
        value["spec"]["compiled_features"],
        serde_json::to_value(&spec.compiled_features).unwrap()
    );
    assert_eq!(value["spec"]["backend"], "vmemory");
    assert_eq!(value["spec"]["workload"], "read-only");
    assert_eq!(value["spec"]["workers"], spec.workers);
    assert_eq!(value["spec"]["repetition"], spec.repetition);
    assert_eq!(value["spec"]["seed"], spec.seed);
}
