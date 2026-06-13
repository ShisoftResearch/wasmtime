#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};
use std::process::Command;

use wasmtime::_internal::transaction_persistence::{
    create_file_backed_storage_for_test, open_file_backed_storage_for_test,
    recover_file_backed_tmemory_for_test,
};
use wasmtime::anyhow::{Context, Result, bail, ensure};
use wasmtime::{Config, Engine, Func, Instance, Module, Store};

fn rust_target_available() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|stdout| stdout.lines().any(|line| line == "wasm32-unknown-unknown"))
}

fn build_guest(manifest: &str, output: &Path) -> Result<()> {
    if !rust_target_available() {
        bail!("wasm32-unknown-unknown target is not installed");
    }

    let status = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-rust",
            "--",
            "build",
            "--manifest-path",
            manifest,
            "--output",
        ])
        .arg(output)
        .status()
        .context("failed to run twasm-rust build")?;

    if !status.success() {
        bail!("twasm-rust build failed with {status}");
    }

    Ok(())
}

fn instantiate(
    path: &Path,
    tmemory_path: &Path,
    tx_log_path: &Path,
    create: bool,
) -> Result<(Store<()>, Instance)> {
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, path)?;
    let mut store = Store::new(&engine, ());

    if create {
        create_file_backed_storage_for_test(
            &mut store,
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
            64,
        )?;
    } else {
        open_file_backed_storage_for_test(
            &mut store,
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
        )?;
    }

    let instance = Instance::new(&mut store, &module, &[])?;
    Ok((store, instance))
}

fn get_func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .with_context(|| format!("missing export {name}"))
}

fn file_backed_tmemory_pages(path: &Path) -> Result<u64> {
    const FILE_BACKED_TMEMORY_PAGE_BYTES: u64 = 512 * 1024;
    let len = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    ensure!(
        len % FILE_BACKED_TMEMORY_PAGE_BYTES == 0,
        "file-backed tmemory length {len} is not page aligned"
    );
    Ok(len / FILE_BACKED_TMEMORY_PAGE_BYTES)
}

#[test]
fn rust_bank_guest_survives_file_backed_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("bank.twasm.wasm");
    let tmemory_path = temp.path().join("bank.tmemory");
    let tx_log_path = temp.path().join("bank.txlog");

    build_guest("examples/transaction-rust/bank/Cargo.toml", &wasm_path)?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &tx_log_path, true)?;
        get_func(&mut store, &instance, "init_bank")?
            .typed::<(i64, i64), ()>(&store)?
            .call(&mut store, (100, 50))?;
        get_func(&mut store, &instance, "transfer")?
            .typed::<(u32, u32, i64), ()>(&store)?
            .call(&mut store, (0, 1, 7))?;
    }

    let tmemory_pages = file_backed_tmemory_pages(&tmemory_path)?;
    recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        tmemory_pages,
        Some(tmemory_pages),
    )?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &tx_log_path, false)?;
        let balance = get_func(&mut store, &instance, "balance")?.typed::<u32, i64>(&store)?;
        assert_eq!(balance.call(&mut store, 0)?, 93);
        assert_eq!(balance.call(&mut store, 1)?, 57);
    }

    Ok(())
}

#[test]
fn rust_pressure_guest_survives_file_backed_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("pressure.twasm.wasm");
    let tmemory_path = temp.path().join("pressure.tmemory");
    let tx_log_path = temp.path().join("pressure.txlog");

    build_guest("examples/transaction-rust/pressure/Cargo.toml", &wasm_path)?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &tx_log_path, true)?;
        let pressure_round =
            get_func(&mut store, &instance, "pressure_round")?.typed::<(u32, u32), i64>(&store)?;
        assert_eq!(pressure_round.call(&mut store, (4, 3))?, 26);
    }

    let tmemory_pages = file_backed_tmemory_pages(&tmemory_path)?;
    recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        tmemory_pages,
        Some(tmemory_pages),
    )?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &tx_log_path, false)?;
        let pressure_round =
            get_func(&mut store, &instance, "pressure_round")?.typed::<(u32, u32), i64>(&store)?;
        assert_eq!(pressure_round.call(&mut store, (0, 3))?, 20);
        assert_eq!(pressure_round.call(&mut store, (4, 3))?, 64);
    }

    Ok(())
}
