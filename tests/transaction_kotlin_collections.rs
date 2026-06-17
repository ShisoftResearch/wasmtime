#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use wasmtime::_internal::transaction_persistence::{
    create_file_backed_storage_for_test, module_fingerprint_bytes_for_test,
    register_module_defined_durable_func_refs_with_fingerprint_for_test,
    reopen_and_recover_file_backed_region,
};
use wasmtime::anyhow::{Context, Result, bail, ensure};
use wasmtime::{Config, Engine, Linker, Module, Store};
use wasmtime_transaction_tools::kotlin_metadata::parse_kotlin_sidecar;
use wasmtime_transaction_tools::kotlin_rewrite::rewrite_kotlin_module;

static KOTLIN_COLLECTIONS_BUILD: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[test]
fn kotlin_collections_execute_and_recover_object_graph() -> Result<()> {
    let Some(wasm_path) = build_kotlin_collections_example()? else {
        println!("skipping kotlin collections execution test: gradle is unavailable");
        return Ok(());
    };

    let sidecar = parse_kotlin_sidecar(
        std::fs::read("examples/transaction-kotlin/collections/twasm.kotlin.json")?.as_slice(),
    )?;
    let input = std::fs::read(&wasm_path)?;
    let (rewritten, report) = rewrite_kotlin_module(&input, &sidecar)?;
    ensure!(
        report.persistent_types > 100,
        "generic mode should discover Kotlin runtime layouts, got {report:?}"
    );

    let temp = tempfile::tempdir()?;
    let tmemory_path = temp.path().join("kotlin-collections.tmemory");
    let tx_log_path = temp.path().join("kotlin-collections.txlog");
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, &rewritten)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx| ctx)?;
    let mut store = Store::new(&engine, wasmtime_wasi::WasiCtxBuilder::new().build_p1());
    create_file_backed_storage_for_test(&mut store, tmemory_path, tx_log_path.clone(), 64)?;
    let instance = linker.instantiate(&mut store, &module)?;
    let module_fingerprint = module_fingerprint_bytes_for_test(&rewritten);
    let registered_funcs = register_module_defined_durable_func_refs_with_fingerprint_for_test(
        &mut store,
        &instance,
        module_fingerprint,
    )?;
    ensure!(
        !registered_funcs.is_empty(),
        "expected Kotlin collections module to register durable identities for defined functions"
    );
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    ensure!(
        recovered.root_object_ids.len() == 1,
        "expected one Profile root, got {:?}",
        recovered.root_object_ids
    );
    ensure!(
        recovered.object_winners.len() >= 6,
        "expected Profile plus String/List/Map/backing object records, got {}",
        recovered.object_winners.len()
    );
    let root_winner = recovered
        .object_winners
        .iter()
        .find(|winner| winner.object_id == recovered.root_object_ids[0])
        .context("root Profile ObjectId did not have a recovered object record")?;
    let name_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 0)?
        .context("recovered Profile name field was null")?;
    let tags_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 1)?
        .context("recovered Profile tags field was null")?;
    let attrs_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 2)?
        .context("recovered Profile attrs field was null")?;
    ensure!(
        name_ref != tags_ref && name_ref != attrs_ref && tags_ref != attrs_ref,
        "Profile name/tags/attrs should point at distinct recovered objects"
    );
    for (label, object_id) in [("name", name_ref), ("tags", tags_ref), ("attrs", attrs_ref)] {
        ensure!(
            recovered
                .object_winners
                .iter()
                .any(|winner| winner.object_id == object_id),
            "Profile {label} field ObjectId {object_id} did not have a recovered object record"
        );
    }
    Ok(())
}

fn recovered_object_ref_field(
    record_bytes: &[u8],
    field_count: usize,
    field_index: usize,
) -> Result<Option<u64>> {
    const OBJECT_VALUE_ABI_TAG_REF: u32 = 5;
    const OBJECT_VALUE_RECORD_LEN: usize = 20;

    ensure!(field_index < field_count, "field index out of range");
    let payload_len = field_count
        .checked_mul(OBJECT_VALUE_RECORD_LEN)
        .context("object payload length overflow")?;
    ensure!(
        record_bytes.len() >= payload_len,
        "recovered object record is shorter than its payload"
    );
    let payload_start = record_bytes.len() - payload_len;
    let field_offset = payload_start + field_index * OBJECT_VALUE_RECORD_LEN;
    let tag = u32::from_le_bytes(
        record_bytes[field_offset..field_offset + 4]
            .try_into()
            .unwrap(),
    );
    let low = u64::from_le_bytes(
        record_bytes[field_offset + 4..field_offset + 12]
            .try_into()
            .unwrap(),
    );
    let high = u64::from_le_bytes(
        record_bytes[field_offset + 12..field_offset + 20]
            .try_into()
            .unwrap(),
    );
    ensure!(
        tag == OBJECT_VALUE_ABI_TAG_REF && high == 0,
        "expected persistent object ref slot, got tag={tag} low={low} high={high}"
    );
    Ok(low.checked_sub(1))
}

fn gradle_available() -> bool {
    Command::new("gradle")
        .arg("--version")
        .status()
        .is_ok_and(|status| status.success())
}

fn build_kotlin_collections_example() -> Result<Option<PathBuf>> {
    let build = KOTLIN_COLLECTIONS_BUILD.get_or_init(|| Mutex::new(None));
    let mut cached = build
        .lock()
        .expect("Kotlin collections build mutex poisoned");
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
        .arg(":collections:clean")
        .arg(":collections:compileDevelopmentExecutableKotlinWasmWasi")
        .current_dir(example_dir)
        .status()
        .with_context(|| format!("failed to run gradle in {}", example_dir.display()))?;

    if !status.success() {
        bail!(
            "gradle :collections:compileDevelopmentExecutableKotlinWasmWasi failed with {status}"
        );
    }

    let wasm_path = find_collections_wasm_artifact(example_dir)?;
    *cached = Some(wasm_path.clone());
    Ok(Some(wasm_path))
}

fn find_collections_wasm_artifact(example_dir: &Path) -> Result<PathBuf> {
    let build_dir =
        example_dir.join("collections/build/compileSync/wasmWasi/main/developmentExecutable");
    let mut wasm_files = Vec::new();
    collect_wasm_files(&build_dir, &mut wasm_files)
        .with_context(|| format!("failed to scan {}", build_dir.display()))?;

    ensure!(
        !wasm_files.is_empty(),
        "Kotlin collections build did not emit a wasm artifact under {}",
        build_dir.display()
    );
    ensure!(
        wasm_files.len() == 1,
        "Kotlin collections build emitted multiple wasm artifacts under {}: {:?}",
        build_dir.display(),
        wasm_files
    );

    Ok(wasm_files.remove(0))
}

fn collect_wasm_files(dir: &Path, wasm_files: &mut Vec<PathBuf>) -> Result<()> {
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
