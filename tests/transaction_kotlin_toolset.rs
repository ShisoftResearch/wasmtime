#![cfg(all(feature = "transaction", unix))]

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use wasmtime::anyhow::{Context, Result, bail, ensure};

static KOTLIN_BANK_BUILD: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();

fn gradle_available() -> bool {
    Command::new("gradle")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn kotlin_bank_example_builds_to_wasm_wasi() -> Result<()> {
    let Some(wasm_path) = build_kotlin_bank_example()? else {
        println!("skipping kotlin bank example build test: gradle is unavailable");
        return Ok(());
    };
    ensure!(
        wasm_path.is_file(),
        "expected Kotlin bank example wasm artifact at {}",
        wasm_path.display()
    );

    Ok(())
}

#[test]
fn twasm_kotlin_inspects_tracked_bank_sidecar() -> Result<()> {
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-kotlin",
            "--",
            "inspect",
            "--metadata",
            "examples/transaction-kotlin/bank/twasm.kotlin.json",
        ])
        .output()
        .context("failed to run twasm-kotlin inspect")?;
    if !output.status.success() {
        bail!(
            "twasm-kotlin inspect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["module"], "transaction-kotlin-bank");
    assert_eq!(report["persistent_types"], 2);
    assert_eq!(report["transaction_functions"], 2);
    assert_eq!(report["roots"], 1);
    Ok(())
}

#[test]
fn twasm_kotlin_inspects_tracked_bank_shape() -> Result<()> {
    let Some(wasm_path) = build_kotlin_bank_example()? else {
        println!("skipping kotlin bank shape inspect test: gradle is unavailable");
        return Ok(());
    };
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-kotlin",
            "--",
            "inspect",
            "--metadata",
            "examples/transaction-kotlin/bank/twasm.kotlin.json",
            "--wasm",
        ])
        .arg(&wasm_path)
        .output()
        .context("failed to run twasm-kotlin inspect with --wasm")?;
    if !output.status.success() {
        bail!(
            "twasm-kotlin inspect --wasm failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["module"], "transaction-kotlin-bank");
    assert!(
        report["wasm_shape"]["gc_struct_types"]
            .as_u64()
            .unwrap_or(0)
            >= 1
    );
    assert!(report["wasm_shape"]["struct_new_ops"].as_u64().unwrap_or(0) >= 1);
    Ok(())
}

#[test]
#[ignore = "Kotlin object lowering not implemented yet"]
fn kotlin_rewrite_output_must_contain_real_transaction_object_ops() -> Result<()> {
    let Some(wasm_path) = build_kotlin_bank_example()? else {
        println!("skipping kotlin bank rewrite test: gradle is unavailable");
        return Ok(());
    };

    let temp = tempfile::tempdir()?;
    let rewritten = temp.path().join("bank.rewritten.wasm");
    let report_path = temp.path().join("bank.rewrite-report.json");
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-kotlin",
            "--",
            "rewrite",
        ])
        .arg(&wasm_path)
        .args([
            "--metadata",
            "examples/transaction-kotlin/bank/twasm.kotlin.json",
            "--output",
        ])
        .arg(&rewritten)
        .args(["--report"])
        .arg(&report_path)
        .output()
        .context("failed to run twasm-kotlin rewrite")?;
    if !output.status.success() {
        bail!(
            "twasm-kotlin rewrite failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let report: serde_json::Value = serde_json::from_slice(&std::fs::read(&report_path)?)?;
    let printed = wasmprinter::print_bytes(std::fs::read(&rewritten)?)?;
    assert!(report["rewritten_object_ops"].as_u64().unwrap_or(0) > 0);
    assert!(
        printed.contains("tstruct.get")
            || printed.contains("tstruct.set")
            || printed.contains("tarray.get")
            || printed.contains("tarray.set")
            || printed.contains("tarray.len")
    );
    Ok(())
}

fn build_kotlin_bank_example() -> Result<Option<std::path::PathBuf>> {
    let build = KOTLIN_BANK_BUILD.get_or_init(|| Mutex::new(None));
    let mut cached = build.lock().expect("Kotlin bank build mutex poisoned");
    if let Some(wasm_path) = cached.as_ref() {
        if wasm_path.is_file() {
            return Ok(Some(wasm_path.clone()));
        }
    }

    if !gradle_available() {
        return Ok(None);
    }

    let example_dir = Path::new("examples/transaction-kotlin");
    let status = Command::new("gradle")
        .arg(":bank:clean")
        .arg(":bank:compileDevelopmentExecutableKotlinWasmWasi")
        .current_dir(example_dir)
        .status()
        .with_context(|| format!("failed to run gradle in {}", example_dir.display()))?;

    if !status.success() {
        bail!("gradle :bank:compileDevelopmentExecutableKotlinWasmWasi failed with {status}");
    }

    let wasm_path = find_bank_wasm_artifact(example_dir)?;
    *cached = Some(wasm_path.clone());
    Ok(Some(wasm_path))
}

fn find_bank_wasm_artifact(example_dir: &Path) -> Result<std::path::PathBuf> {
    let build_dir = example_dir.join("bank/build/compileSync/wasmWasi/main/developmentExecutable");
    let mut wasm_files = Vec::new();
    collect_wasm_files(&build_dir, &mut wasm_files)
        .with_context(|| format!("failed to scan {}", build_dir.display()))?;

    ensure!(
        !wasm_files.is_empty(),
        "Kotlin bank build did not emit a wasm artifact under {}",
        build_dir.display()
    );
    ensure!(
        wasm_files.len() == 1,
        "Kotlin bank build emitted multiple wasm artifacts under {}: {:?}",
        build_dir.display(),
        wasm_files
    );

    Ok(wasm_files.remove(0))
}

fn collect_wasm_files(dir: &Path, wasm_files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_wasm_files(&path, wasm_files)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "wasm")
        {
            wasm_files.push(path);
        }
    }
    Ok(())
}
