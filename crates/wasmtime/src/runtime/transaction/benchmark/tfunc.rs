use super::config::{
    BackendKind, BenchmarkRequest, compiled_concurrency_control_name, compiled_policy_name,
    compiled_transaction_features, compiled_visibility_mode,
};
use super::metrics::{CellAggregateInput, CellMetrics, GcMetrics, VersionMetrics, WorkerMetrics};
use super::record::{SCHEMA_VERSION, TfuncControlRecord};
use super::workload::DeterministicRng;
use crate::prelude::*;
use std::path::Path;
use std::time::{Duration, Instant};

const TRANSACTION_CONTROL_MODULE: &str = r#"
    (module
      (tmemory 1)
      (tfunc (export "read") (param i32) (result i64)
        (i64.tload (local.get 0)))
      (tfunc (export "rmw") (result i64)
        (local $next i64)
        (local.set $next
          (i64.add (i64.tload (i32.const 0)) (i64.const 1)))
        (i64.tstore (i32.const 0) (local.get $next))
        (local.get $next)))
"#;

const READ_GRANULES: u64 = 256;
const GRANULE_BYTES: u64 = 64;
const READ_ADDRESS_COUNT: usize = 8;

pub(super) fn run_tfunc_controls(
    request: &BenchmarkRequest,
    backend: BackendKind,
) -> Result<Vec<TfuncControlRecord>> {
    run_tfunc_controls_in(request, backend, None)
}

fn run_tfunc_controls_in(
    request: &BenchmarkRequest,
    backend: BackendKind,
    temporary_parent: Option<&Path>,
) -> Result<Vec<TfuncControlRecord>> {
    request.validate()?;
    let mut records = Vec::with_capacity(request.repetitions as usize * 2);
    for repetition in 0..request.repetitions {
        records.extend(run_tfunc_repetition(
            request,
            backend,
            repetition,
            temporary_parent,
        )?);
    }
    Ok(records)
}

fn run_tfunc_repetition(
    request: &BenchmarkRequest,
    backend: BackendKind,
    repetition: u32,
    temporary_parent: Option<&Path>,
) -> Result<[TfuncControlRecord; 2]> {
    let _cleanup = crate::runtime::transaction::clear_current_thread_transaction_on_drop_for_test();
    let engine = crate::Engine::default();
    let module = crate::Module::new(&engine, wat::parse_str(TRANSACTION_CONTROL_MODULE)?)?;
    // Keep the backing directory alive until after the store and its durable
    // mappings have been dropped. Rust drops locals in reverse declaration
    // order, including when a control exits through an error.
    let directory = match backend {
        BackendKind::Vmemory => None,
        BackendKind::FileBacked => Some(match temporary_parent {
            Some(parent) => tempfile::Builder::new()
                .prefix("wasmtime-transaction-tfunc-benchmark-")
                .tempdir_in(parent)?,
            None => tempfile::Builder::new()
                .prefix("wasmtime-transaction-tfunc-benchmark-")
                .tempdir()?,
        }),
    };
    let mut store = crate::Store::new(&engine, ());
    if let Some(directory) = &directory {
        store.transaction_create_file_backed_storage_for_test(
            directory.path().join("tmemory.bin"),
            directory.path().join("transaction-log.bin"),
            4096,
        )?;
    }
    let instance = crate::Instance::new(&mut store, &module, &[])?;
    let read = instance.get_typed_func::<i32, i64>(&mut store, "read")?;
    let rmw = instance.get_typed_func::<(), i64>(&mut store, "rmw")?;
    let warmup = Duration::from_millis(request.warmup_ms);
    let measurement = Duration::from_millis(request.measure_ms);

    let read_addresses = deterministic_read_addresses(request.seed)?;
    let mut read_index = 0usize;
    run_until(warmup, || {
        read.call(&mut store, read_addresses[read_index])?;
        read_index = (read_index + 1) % read_addresses.len();
        Ok(())
    })?;
    let read_metrics = measure_reads(measurement, &read_addresses, read_index, &read, &mut store)?;

    run_until(warmup, || rmw.call(&mut store, ()).map(|_| ()))?;
    let baseline = read.call(&mut store, 0)?;
    let rmw_metrics = measure_rmw(measurement, &rmw, &mut store)?;
    let final_counter = read.call(&mut store, 0)?;
    let expected_delta = i64::try_from(rmw_metrics.counts.committed_operations)
        .context("tfunc RMW commit count does not fit i64")?;
    ensure!(
        final_counter.checked_sub(baseline) == Some(expected_delta),
        "tfunc RMW counter delta {} does not equal committed operations {}",
        final_counter.saturating_sub(baseline),
        rmw_metrics.counts.committed_operations
    );

    Ok([
        control_record(backend, "read-only", repetition, request.seed, read_metrics),
        control_record(backend, "rmw", repetition, request.seed, rmw_metrics),
    ])
}

fn read_address(rng: &mut DeterministicRng) -> Result<i32> {
    let address = (rng.next_u64() % READ_GRANULES)
        .checked_mul(GRANULE_BYTES)
        .context("tfunc read address overflow")?;
    i32::try_from(address).context("tfunc read address does not fit i32")
}

fn deterministic_read_addresses(seed: u64) -> Result<[i32; READ_ADDRESS_COUNT]> {
    let mut rng = DeterministicRng::new(seed, 0);
    let mut addresses = [0; READ_ADDRESS_COUNT];
    for address in &mut addresses {
        *address = read_address(&mut rng)?;
    }
    Ok(addresses)
}

fn run_until(duration: Duration, mut operation: impl FnMut() -> Result<()>) -> Result<()> {
    let started = Instant::now();
    let deadline = checked_deadline(started, duration, "tfunc warmup")?;
    loop {
        operation()?;
        if Instant::now() >= deadline {
            break;
        }
    }
    ensure!(
        !started.elapsed().is_zero(),
        "tfunc benchmark phase did not advance time"
    );
    Ok(())
}

fn measure_reads(
    requested: Duration,
    addresses: &[i32; READ_ADDRESS_COUNT],
    mut address_index: usize,
    read: &crate::TypedFunc<i32, i64>,
    store: &mut crate::Store<()>,
) -> Result<CellMetrics> {
    let mut worker = WorkerMetrics::default();
    let mut checksum = 0i64;
    let started = Instant::now();
    let deadline = checked_deadline(started, requested, "tfunc read measurement")?;
    loop {
        let address = addresses[address_index];
        address_index = (address_index + 1) % addresses.len();
        let call_started = Instant::now();
        let value = read.call(&mut *store, address)?;
        let elapsed = call_started.elapsed();
        checksum = checksum.wrapping_add(value);
        worker.record_commit(elapsed)?;
        if Instant::now() >= deadline {
            break;
        }
    }
    ensure!(
        checksum == 0,
        "tfunc read-only checksum changed: {checksum}"
    );
    aggregate_metrics(requested, started.elapsed(), worker)
}

fn measure_rmw(
    requested: Duration,
    rmw: &crate::TypedFunc<(), i64>,
    store: &mut crate::Store<()>,
) -> Result<CellMetrics> {
    let mut worker = WorkerMetrics::default();
    let started = Instant::now();
    let deadline = checked_deadline(started, requested, "tfunc RMW measurement")?;
    loop {
        let operation_started = Instant::now();
        let _next = rmw.call(&mut *store, ())?;
        let elapsed = operation_started.elapsed();
        worker.record_commit(elapsed)?;
        if Instant::now() >= deadline {
            break;
        }
    }
    aggregate_metrics(requested, started.elapsed(), worker)
}

fn checked_deadline(started: Instant, duration: Duration, phase: &str) -> Result<Instant> {
    started
        .checked_add(duration)
        .with_context(|| format!("{phase} deadline overflow"))
}

fn aggregate_metrics(
    requested: Duration,
    effective: Duration,
    worker: WorkerMetrics,
) -> Result<CellMetrics> {
    let metrics = CellMetrics::from_workers(
        &[worker],
        CellAggregateInput {
            requested_elapsed: requested,
            effective_elapsed: effective,
            gc: GcMetrics {
                policy_mode: super::selected_gc_policy_mode()?.into(),
                ..GcMetrics::default()
            },
            versions: VersionMetrics::default(),
            ..CellAggregateInput::default()
        },
    )?;
    metrics.validate_finite()?;
    Ok(metrics)
}

fn control_record(
    backend: BackendKind,
    control: &str,
    repetition: u32,
    seed: u64,
    metrics: CellMetrics,
) -> TfuncControlRecord {
    TfuncControlRecord {
        schema_version: SCHEMA_VERSION,
        policy: compiled_policy_name().into(),
        visibility_mode: compiled_visibility_mode().into(),
        concurrency_control: compiled_concurrency_control_name().into(),
        compiled_features: compiled_transaction_features()
            .iter()
            .map(|feature| (*feature).into())
            .collect(),
        backend,
        control: control.into(),
        repetition,
        seed,
        metrics,
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::compiled_policy_name;
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn controls_validate_read_checksum_and_rmw_delta_for_both_backends() -> Result<()> {
        let mut request = BenchmarkRequest::quick(compiled_policy_name().into());
        request.warmup_ms = 1;
        request.measure_ms = 5;
        request.repetitions = 1;

        for backend in [BackendKind::Vmemory, BackendKind::FileBacked] {
            let records = run_tfunc_controls(&request, backend)?;
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].control, "read-only");
            assert_eq!(records[1].control, "rmw");
            assert!(records.iter().all(|record| {
                record.backend == backend && record.metrics.counts.committed_operations > 0
            }));
        }
        Ok(())
    }

    #[test]
    fn file_backed_control_removes_its_temporary_storage_after_store_drop() -> Result<()> {
        let mut request = BenchmarkRequest::quick(compiled_policy_name().into());
        request.warmup_ms = 1;
        request.measure_ms = 1;
        request.repetitions = 1;
        let parent = tempfile::tempdir()?;

        run_tfunc_controls_in(&request, BackendKind::FileBacked, Some(parent.path()))?;

        assert_eq!(std::fs::read_dir(parent.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn extreme_duration_returns_an_error_instead_of_panicking() {
        let outcome = catch_unwind(AssertUnwindSafe(|| run_until(Duration::MAX, || Ok(()))));

        let result = outcome.expect("extreme tfunc duration must not panic");
        let error = result.unwrap_err().to_string();
        assert!(error.contains("deadline overflow"), "{error}");
    }

    #[test]
    fn controls_reject_overlong_finite_phases_before_starting_warmup() {
        let mut request = BenchmarkRequest::quick(compiled_policy_name().into());
        request.warmup_ms = 24 * 60 * 60 * 1_000 + 1;
        request.measure_ms = 1;
        request.repetitions = 1;
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = run_tfunc_controls(&request, BackendKind::Vmemory)
                .map(|_| ())
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });

        let result = receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("overlong tfunc request started executing instead of failing validation");
        worker.join().unwrap();
        let error = result.unwrap_err();
        assert!(error.contains("at most"), "{error}");
    }
}
