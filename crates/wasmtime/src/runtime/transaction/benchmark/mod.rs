mod config;
mod metrics;
mod record;
mod runner;
mod storage;
mod tfunc;
mod workload;

use self::config::{
    BenchmarkRequest, CellSpec, MatrixDisposition, compiled_policy_name,
    compiled_transaction_features,
};
use self::record::{
    BenchmarkRecord, CellRecord, CompleteRecord, DriverRecord, FailureRecord, JsonlWriter,
    SCHEMA_VERSION, SkippedCellRecord,
};
use self::runner::{cell_failure_record, run_cell};
use self::tfunc::run_tfunc_controls;
use super::{GcMvccMode, TransactionRegionRuntime};
use crate::prelude::*;
use std::path::Path;

fn run_driver(request_path: &Path, output_path: &Path) -> Result<()> {
    let available_parallelism = std::thread::available_parallelism()
        .map(usize::from)
        .context("failed to determine available parallelism")?;
    run_driver_with_cell_runner(request_path, output_path, available_parallelism, run_cell)
}

fn run_driver_with_cell_runner(
    request_path: &Path,
    output_path: &Path,
    available_parallelism: usize,
    mut cell_runner: impl FnMut(&BenchmarkRequest, &CellSpec) -> Result<CellRecord>,
) -> Result<()> {
    let request = BenchmarkRequest::read(request_path)?;
    request.validate()?;
    let gc_policy_mode = selected_gc_policy_mode()?;
    let mut writer = JsonlWriter::create(output_path)?;
    let compiled_features = compiled_transaction_features()
        .iter()
        .map(|feature| (*feature).to_string())
        .collect::<Vec<_>>();
    writer.write(&BenchmarkRecord::Driver(DriverRecord {
        schema_version: SCHEMA_VERSION,
        compiled_policy: compiled_policy_name().into(),
        compiled_features,
        available_parallelism,
        gc_policy_mode: gc_policy_mode.into(),
    }))?;

    let mut completed_cells = 0usize;
    let mut skipped_cells = 0usize;
    for entry in request.cells(available_parallelism) {
        match entry.disposition {
            MatrixDisposition::Run => match cell_runner(&request, &entry.spec) {
                Ok(record) => {
                    writer.write(&BenchmarkRecord::Cell(record))?;
                    completed_cells += 1;
                }
                Err(error) => {
                    write_cell_failure(&mut writer, &entry.spec, &error)?;
                    return Err(error);
                }
            },
            MatrixDisposition::Skipped(reason) => {
                writer.write(&BenchmarkRecord::SkippedCell(SkippedCellRecord {
                    schema_version: SCHEMA_VERSION,
                    spec: entry.spec,
                    reason,
                }))?;
                skipped_cells += 1;
            }
        }
    }

    let mut tfunc_controls = 0usize;
    if request.include_tfunc_control {
        for &backend in &request.backends {
            let controls = match run_tfunc_controls(&request, backend) {
                Ok(controls) => controls,
                Err(error) => {
                    write_failure(&mut writer, None, "tfunc-control", &error)?;
                    return Err(error);
                }
            };
            for control in controls {
                writer.write(&BenchmarkRecord::TfuncControl(control))?;
                tfunc_controls += 1;
            }
        }
    }

    writer.write(&BenchmarkRecord::Complete(CompleteRecord {
        schema_version: SCHEMA_VERSION,
        policy: compiled_policy_name().into(),
        completed_cells,
        skipped_cells,
        tfunc_controls,
    }))?;
    Ok(())
}

fn write_cell_failure(writer: &mut JsonlWriter, spec: &CellSpec, error: &Error) -> Result<()> {
    if let Some(failure) = cell_failure_record(error) {
        ensure!(
            failure.cell.as_ref() == Some(spec),
            "typed benchmark cell failure does not match the active cell"
        );
        writer.write(&BenchmarkRecord::Failure(failure.clone()))
    } else {
        write_failure(writer, Some(spec.clone()), "cell", error)
    }
}

fn write_failure(
    writer: &mut JsonlWriter,
    cell: Option<config::CellSpec>,
    phase: &str,
    error: &Error,
) -> Result<()> {
    writer.write(&BenchmarkRecord::Failure(FailureRecord {
        schema_version: SCHEMA_VERSION,
        policy: compiled_policy_name().into(),
        cell,
        phase: phase.into(),
        message: format!("{error:#}"),
    }))
}

fn selected_gc_policy_mode() -> Result<&'static str> {
    match TransactionRegionRuntime::new_for_test().persistent_gc_mode_for_test()? {
        GcMvccMode::CurrentStateOnly => Ok("current-state-only"),
        GcMvccMode::MvccCompliant => Ok("mvcc-compliant"),
    }
}

#[test]
#[ignore = "explicit release-mode transaction benchmark"]
fn transaction_cc_benchmark_driver() -> Result<()> {
    let request_path = std::env::var_os("WASMTIME_TRANSACTION_BENCH_REQUEST")
        .context("WASMTIME_TRANSACTION_BENCH_REQUEST is not set")?;
    let output_path = std::env::var_os("WASMTIME_TRANSACTION_BENCH_OUTPUT")
        .context("WASMTIME_TRANSACTION_BENCH_OUTPUT is not set")?;
    run_driver(Path::new(&request_path), Path::new(&output_path))
}

#[cfg(test)]
mod tests {
    use super::config::{BackendKind, BenchmarkRequest, WorkloadKind, compiled_policy_name};
    use super::*;

    #[test]
    fn release_driver_smoke() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let request_path = directory.path().join("request.json");
        let output_path = directory.path().join("output.jsonl");
        let request = BenchmarkRequest {
            schema_version: 1,
            expected_policy: compiled_policy_name().into(),
            warmup_ms: 1,
            measure_ms: 5,
            repetitions: 1,
            backends: vec![BackendKind::Vmemory],
            workloads: vec![WorkloadKind::ReadOnly],
            workers: vec![1],
            seed: 7,
            gc_commit_interval: 4096,
            include_tfunc_control: true,
        };
        std::fs::write(&request_path, serde_json::to_vec(&request)?)?;

        run_driver(&request_path, &output_path)?;

        let records = std::fs::read_to_string(&output_path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let kinds = records
            .iter()
            .map(|record| record["record_type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                "driver",
                "cell",
                "tfunc_control",
                "tfunc_control",
                "complete"
            ]
        );
        assert_eq!(records[0]["compiled_policy"], compiled_policy_name());
        assert_eq!(
            records[0]["gc_policy_mode"], records[1]["metrics"]["gc"]["policy_mode"],
            "driver GC mode must match the selected pluggable collector"
        );
        assert_eq!(
            records[0]["gc_policy_mode"], records[2]["metrics"]["gc"]["policy_mode"],
            "tfunc GC identity must match the selected pluggable collector"
        );
        assert_eq!(records.last().unwrap()["record_type"], "complete");
        Ok(())
    }

    #[test]
    fn all_skipped_driver_rejects_overlong_phases_before_writing_output() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let request_path = directory.path().join("request.json");
        let output_path = directory.path().join("output.jsonl");
        let mut request = BenchmarkRequest::quick(compiled_policy_name().into());
        request.warmup_ms = 24 * 60 * 60 * 1_000 + 1;
        request.measure_ms = 1;
        request.backends = vec![BackendKind::Vmemory];
        request.workloads = vec![WorkloadKind::ReadOnly];
        request.workers = vec![usize::MAX];
        request.include_tfunc_control = false;
        std::fs::write(&request_path, serde_json::to_vec(&request)?)?;

        let error = run_driver(&request_path, &output_path).unwrap_err();

        assert!(format!("{error:#}").contains("at most"));
        assert!(!output_path.exists());
        Ok(())
    }

    #[test]
    fn driver_streams_exact_cell_failure_phase_after_prior_records() -> Result<()> {
        for phase in [
            "worker-startup",
            "warmup-and-measurement",
            "fixed-write-skew",
        ] {
            let directory = tempfile::tempdir()?;
            let request_path = directory.path().join("request.json");
            let output_path = directory.path().join("output.jsonl");
            let mut request = BenchmarkRequest::quick(compiled_policy_name().into());
            request.warmup_ms = 1;
            request.measure_ms = 1;
            request.backends = vec![BackendKind::Vmemory];
            request.workloads = vec![WorkloadKind::ReadOnly];
            request.workers = vec![usize::MAX, 1];
            request.include_tfunc_control = false;
            std::fs::write(&request_path, serde_json::to_vec(&request)?)?;

            let error =
                run_driver_with_cell_runner(&request_path, &output_path, 1, |_request, spec| {
                    Err(runner::cell_failure(
                        spec,
                        phase,
                        "injected driver failure".into(),
                    ))
                })
                .unwrap_err();
            assert!(format!("{error:#}").contains("injected driver failure"));

            let records = std::fs::read_to_string(&output_path)?
                .lines()
                .map(serde_json::from_str::<serde_json::Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let kinds = records
                .iter()
                .map(|record| record["record_type"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(kinds, ["driver", "skipped_cell", "failure"]);
            assert_eq!(records[1]["spec"]["workers"], usize::MAX);
            assert_eq!(records[2]["phase"], phase);
            assert_eq!(records[2]["cell"]["workers"], 1);
            assert_eq!(records[2]["message"], "injected driver failure");
            assert!(
                !records
                    .iter()
                    .any(|record| record["record_type"] == "complete")
            );
        }
        Ok(())
    }
}
