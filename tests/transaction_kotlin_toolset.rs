#![cfg(all(feature = "transaction", unix))]

use std::path::Path;
use std::process::Command;

use wasmtime::anyhow::{Context, Result, bail, ensure};

fn gradle_available() -> bool {
    Command::new("gradle")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn kotlin_bank_example_builds_to_wasm_wasi() -> Result<()> {
    if !gradle_available() {
        println!("skipping kotlin bank example build test: gradle is unavailable");
        return Ok(());
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
    ensure!(
        wasm_path.is_file(),
        "expected Kotlin bank example wasm artifact at {}",
        wasm_path.display()
    );

    Ok(())
}

fn find_bank_wasm_artifact(example_dir: &Path) -> Result<std::path::PathBuf> {
    let build_dir = example_dir.join("bank/build");
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
