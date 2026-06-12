#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};

use tempfile::tempdir;
use wasmtime::_internal::transaction_persistence::{
    fail_next_commit_before_lp_for_test, recover_file_backed_tmemory_for_test,
};
use wasmtime::{Config, Engine, Result};
use wasmtime_wast::{Async, WastContext};

#[test]
fn transaction_wast_commit_survives_file_backed_recovery() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");

    let engine = transaction_wast_engine()?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-commit-before.wast"),
        &tmemory_path,
        &tx_log_path,
        true,
        false,
    )?;

    recover_file_backed_tmemory_for_test(&tx_log_path, &tmemory_path, 1, Some(1))?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-commit-after.wast"),
        &tmemory_path,
        &tx_log_path,
        false,
        false,
    )?;

    Ok(())
}

#[test]
fn transaction_wast_trap_does_not_recover_staged_tmemory_write() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");

    let engine = transaction_wast_engine()?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-abort-before.wast"),
        &tmemory_path,
        &tx_log_path,
        true,
        false,
    )?;

    recover_file_backed_tmemory_for_test(&tx_log_path, &tmemory_path, 1, Some(1))?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-abort-after.wast"),
        &tmemory_path,
        &tx_log_path,
        false,
        false,
    )?;

    Ok(())
}

#[test]
fn transaction_wast_loose_end_tmemory_write_is_recovered() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");

    let engine = transaction_wast_engine()?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-loose-end-before.wast"),
        &tmemory_path,
        &tx_log_path,
        true,
        true,
    )?;

    let tmemory_bytes = std::fs::read(&tmemory_path)?;
    assert_eq!(&tmemory_bytes[64..68], &[0x88, 0x77, 0x66, 0x55]);

    recover_file_backed_tmemory_for_test(&tx_log_path, &tmemory_path, 1, Some(1))?;

    run_wast_phase(
        &engine,
        &fixture_path("tmemory-loose-end-after.wast"),
        &tmemory_path,
        &tx_log_path,
        false,
        false,
    )?;

    Ok(())
}

fn transaction_wast_engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    Engine::new(&config)
}

fn run_wast_phase(
    engine: &Engine,
    wast_path: &Path,
    tmemory_path: &Path,
    tx_log_path: &Path,
    create: bool,
    arm_fail_next_commit_before_lp_for_test: bool,
) -> Result<()> {
    let tmemory_path = tmemory_path.to_path_buf();
    let tx_log_path = tx_log_path.to_path_buf();
    let mut context = WastContext::new(engine, Async::No, move |store| {
        let result = if create {
            store.transaction_create_file_backed_storage_for_test(
                tmemory_path.clone(),
                tx_log_path.clone(),
                64,
            )
        } else {
            store.transaction_open_file_backed_storage_for_test(
                tmemory_path.clone(),
                tx_log_path.clone(),
            )
        };
        result.unwrap_or_else(|error| {
            panic!("failed to configure file-backed transaction storage: {error:#}");
        });
        if arm_fail_next_commit_before_lp_for_test {
            fail_next_commit_before_lp_for_test(store);
        }
    });
    context.run_file(wast_path)
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/transaction-persistence")
        .join(name)
}
