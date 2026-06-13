#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};
use std::process::Command;

use wasmparser::{Operator, Parser, Payload};
use wasmtime::anyhow::{Context, Result, bail};
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

#[derive(Default)]
struct TransactionShape {
    i32_tloads: usize,
    i64_tloads: usize,
    f32_tloads: usize,
    f64_tloads: usize,
    i32_tstores: usize,
    i64_tstores: usize,
    f32_tstores: usize,
    f64_tstores: usize,
    marker_imports: usize,
}

fn inspect_transaction_shape(bytes: &[u8]) -> Result<TransactionShape> {
    let mut shape = TransactionShape::default();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.context("failed to parse rewritten Rust guest")? {
            Payload::ImportSection(imports) => {
                for import_group in imports {
                    let import_group = import_group.context("failed to parse import group")?;
                    count_marker_imports(import_group, &mut shape)?;
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut reader = body.get_operators_reader()?;
                while !reader.eof() {
                    match reader.read()? {
                        Operator::I32TLoad { .. } => shape.i32_tloads += 1,
                        Operator::I64TLoad { .. } => shape.i64_tloads += 1,
                        Operator::F32TLoad { .. } => shape.f32_tloads += 1,
                        Operator::F64TLoad { .. } => shape.f64_tloads += 1,
                        Operator::I32TStore { .. } => shape.i32_tstores += 1,
                        Operator::I64TStore { .. } => shape.i64_tstores += 1,
                        Operator::F32TStore { .. } => shape.f32_tstores += 1,
                        Operator::F64TStore { .. } => shape.f64_tstores += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(shape)
}

fn count_marker_imports(
    imports: wasmparser::Imports<'_>,
    shape: &mut TransactionShape,
) -> Result<()> {
    match imports {
        wasmparser::Imports::Single(_, import) => {
            if import.module == "twasm_intrinsics" {
                shape.marker_imports += 1;
            }
        }
        wasmparser::Imports::Compact1 { module, items } => {
            for item in items {
                let _item = item.context("failed to parse compact import")?;
                if module == "twasm_intrinsics" {
                    shape.marker_imports += 1;
                }
            }
        }
        wasmparser::Imports::Compact2 { module, names, .. } => {
            for name in names {
                let _name = name.context("failed to parse compact import name")?;
                if module == "twasm_intrinsics" {
                    shape.marker_imports += 1;
                }
            }
        }
    }

    Ok(())
}

fn instantiate_with_wasmtime_variant(wasm_path: &Path) -> Result<(Store<()>, Instance)> {
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, wasm_path)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    Ok((store, instance))
}

fn get_func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .with_context(|| format!("missing export {name}"))
}

#[test]
fn rust_pressure_postpass_outputs_all_scalar_transaction_ops() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("pressure.twasm.wasm");
    build_guest("examples/transaction-rust/pressure/Cargo.toml", &wasm_path)?;

    let bytes = std::fs::read(&wasm_path)?;
    let wat = wasmprinter::print_bytes(&bytes)?;
    let shape = inspect_transaction_shape(&bytes)?;

    assert_eq!(
        shape.marker_imports, 0,
        "SDK marker imports must be removed by the postpass"
    );
    assert!(
        shape.i32_tloads > 0,
        "rewritten Rust guest should contain i32.tload"
    );
    assert!(
        shape.i64_tloads > 0,
        "rewritten Rust guest should contain i64.tload"
    );
    assert!(
        shape.f32_tloads > 0,
        "rewritten Rust guest should contain f32.tload"
    );
    assert!(
        shape.f64_tloads > 0,
        "rewritten Rust guest should contain f64.tload"
    );
    assert!(
        shape.i32_tstores > 0,
        "rewritten Rust guest should contain i32.tstore"
    );
    assert!(
        shape.i64_tstores > 0,
        "rewritten Rust guest should contain i64.tstore"
    );
    assert!(
        shape.f32_tstores > 0,
        "rewritten Rust guest should contain f32.tstore"
    );
    assert!(
        shape.f64_tstores > 0,
        "rewritten Rust guest should contain f64.tstore"
    );
    assert!(
        wat.contains("i32.tload"),
        "printed WAT should expose i32.tload syntax"
    );
    assert!(
        wat.contains("i64.tload"),
        "printed WAT should expose i64.tload syntax"
    );
    assert!(
        wat.contains("f32.tload"),
        "printed WAT should expose f32.tload syntax"
    );
    assert!(
        wat.contains("f64.tload"),
        "printed WAT should expose f64.tload syntax"
    );
    assert!(
        wat.contains("i32.tstore"),
        "printed WAT should expose i32.tstore syntax"
    );
    assert!(
        wat.contains("i64.tstore"),
        "printed WAT should expose i64.tstore syntax"
    );
    assert!(
        wat.contains("f32.tstore"),
        "printed WAT should expose f32.tstore syntax"
    );
    assert!(
        wat.contains("f64.tstore"),
        "printed WAT should expose f64.tstore syntax"
    );
    assert!(
        !wat.contains("twasm_intrinsics"),
        "printed WAT must not retain SDK marker ABI"
    );

    Ok(())
}

#[test]
fn rust_bank_postpass_runs_on_wasmtime_transaction_variant() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("bank.twasm.wasm");
    build_guest("examples/transaction-rust/bank/Cargo.toml", &wasm_path)?;

    let (mut store, instance) = instantiate_with_wasmtime_variant(&wasm_path)?;

    get_func(&mut store, &instance, "init_bank")?
        .typed::<(i64, i64), ()>(&store)?
        .call(&mut store, (100, 50))?;
    get_func(&mut store, &instance, "transfer")?
        .typed::<(u32, u32, i64), ()>(&store)?
        .call(&mut store, (0, 1, 7))?;

    let balance = get_func(&mut store, &instance, "balance")?.typed::<u32, i64>(&store)?;
    assert_eq!(balance.call(&mut store, 0)?, 93);
    assert_eq!(balance.call(&mut store, 1)?, 57);

    Ok(())
}
