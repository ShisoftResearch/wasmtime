#![cfg(all(feature = "transaction", unix))]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use wasmparser::{BinaryReader, KnownCustom, Name, Operator, Parser, Payload};
use wasmtime::_internal::transaction_persistence::{
    create_file_backed_storage_for_test, enable_live_wast_reference_fallbacks_for_test,
    reopen_and_recover_file_backed_region,
};
use wasmtime::anyhow::{Context, Result, bail, ensure};
use wasmtime::{Config, Engine, Instance, Linker, Module, Store};
use wasmtime_transaction_tools::kotlin_metadata::parse_kotlin_sidecar;
use wasmtime_transaction_tools::kotlin_rewrite::rewrite_kotlin_module;

static KOTLIN_BANK_BUILD: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";

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
    let rewritten_bytes = std::fs::read(&rewritten)?;
    let shape = kotlin_transaction_shape(&rewritten_bytes)?;
    assert!(report["rewritten_object_ops"].as_u64().unwrap_or(0) > 0);
    ensure!(
        shape.marker_imports == 0,
        "Kotlin marker imports must be lowered before execution: {shape:?}"
    );
    ensure!(
        shape.tfuncs >= 2,
        "expected transfer and installFreshBank to be transaction functions: {shape:?}"
    );
    ensure!(
        shape.tfunc_names.contains("transfer"),
        "expected transfer to be recorded as a transaction function: {shape:?}"
    );
    ensure!(
        shape.tfunc_names.contains("installFreshBank"),
        "expected installFreshBank to be recorded as a transaction function: {shape:?}"
    );
    ensure!(
        shape.tstruct_gets + shape.tstruct_sets > 0,
        "expected real transactional struct operations: {shape:?}"
    );

    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    Module::new(&engine, &rewritten_bytes)?;
    Ok(())
}

#[test]
fn kotlin_rewritten_bank_executes_and_recovers_file_backed_roots() -> Result<()> {
    let Some(wasm_path) = build_kotlin_bank_example()? else {
        println!("skipping kotlin bank execution test: gradle is unavailable");
        return Ok(());
    };

    let sidecar = parse_kotlin_sidecar(
        std::fs::read("examples/transaction-kotlin/bank/twasm.kotlin.json")?.as_slice(),
    )?;
    let input = std::fs::read(&wasm_path)?;
    let (rewritten, _) = rewrite_kotlin_module(&input, &sidecar)?;

    let temp = tempfile::tempdir()?;
    let tmemory_path = temp.path().join("kotlin-bank.tmemory");
    let tx_log_path = temp.path().join("kotlin-bank.txlog");
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
    // SHISOFT-TWASM-MOCK: Kotlin runtime object headers carry live funcrefs
    // today. Persisted user fields are validated below; symbolic Kotlin funcref
    // rebinding remains a separate runtime metadata workstream.
    enable_live_wast_reference_fallbacks_for_test(&mut store);
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    ensure!(
        recovered.root_object_ids.len() == 1,
        "expected one recovered Kotlin root, got {:?}",
        recovered.root_object_ids
    );
    ensure!(
        recovered.object_winners.len() >= 3,
        "expected recovered Bank and Account records, got {}",
        recovered.object_winners.len()
    );
    let root_winner = recovered
        .object_winners
        .iter()
        .find(|winner| winner.object_id == recovered.root_object_ids[0])
        .context("root ObjectId did not have a recovered object record")?;
    let alice_ref = recovered_object_ref_field(&root_winner.record_bytes, 2, 0)?
        .context("recovered Bank alice field was null")?;
    let bob_ref = recovered_object_ref_field(&root_winner.record_bytes, 2, 1)?
        .context("recovered Bank bob field was null")?;
    ensure!(alice_ref != bob_ref, "alice and bob should be distinct accounts");
    let alice = recovered
        .object_winners
        .iter()
        .find(|winner| winner.object_id == alice_ref)
        .context("recovered alice ObjectId did not have an Account record")?;
    let bob = recovered
        .object_winners
        .iter()
        .find(|winner| winner.object_id == bob_ref)
        .context("recovered bob ObjectId did not have an Account record")?;
    ensure!(
        recovered_object_i64_field(&alice.record_bytes, 1, 0)? == 900,
        "alice balance was not recovered after transfer"
    );
    ensure!(
        recovered_object_i64_field(&bob.record_bytes, 1, 0)? == 300,
        "bob balance was not recovered after transfer"
    );
    Ok(())
}

#[test]
fn kotlin_promotes_ordinary_wasmgc_objects() -> Result<()> {
    let input = wat::parse_str(
        r#"
        (module
          (type $account (struct (field (mut i64))))
          (type $accounts (array (mut (ref null $account))))
          (type $bank
            (struct
              (field (mut (ref null $account)))
              (field (mut (ref null $account)))
              (field (mut (ref null $accounts)))))
          (tglobal $root (mut (ref null $bank)) (ref.null $bank))
          (func (export "publish") (local $alice (ref $account))
            i64.const 100
            struct.new $account
            local.tee $alice
            local.get $alice
            local.get $alice
            i32.const 2
            array.new $accounts
            struct.new $bank
            tglobal.set $root))
        "#,
    )?;
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "kotlin-shaped-promotion",
          "persistentTypes": [
            {
              "name": "Account",
              "kind": "struct",
              "fields": [
                { "name": "balance", "kind": "i64", "nullable": false }
              ]
            },
            {
              "name": "Accounts",
              "kind": "array",
              "element": {
                "name": "element",
                "kind": "ref",
                "type": "Account",
                "nullable": true
              }
            },
            {
              "name": "Bank",
              "kind": "struct",
              "fields": [
                { "name": "alice", "kind": "ref", "type": "Account", "nullable": true },
                { "name": "mirror", "kind": "ref", "type": "Account", "nullable": true },
                { "name": "accounts", "kind": "ref", "type": "Accounts", "nullable": true }
              ]
            }
          ],
          "transactionFunctions": ["publish"],
          "roots": [
            { "name": "bank", "type": "Bank", "nullable": true }
          ]
        }"#
        .as_slice(),
    )?;

    let (rewritten, report) = rewrite_kotlin_module(&input, &sidecar)?;
    ensure!(
        report.rewritten_tfuncs == 1,
        "expected publish to be marked"
    );
    ensure!(
        report.rewritten_object_ops == 0,
        "ordinary constructors should remain ordinary until promotion: {report:?}"
    );
    let printed = wasmprinter::print_bytes(&rewritten)?;
    ensure!(
        printed.contains("struct.new"),
        "missing ordinary struct.new"
    );
    ensure!(printed.contains("array.new"), "missing ordinary array.new");
    ensure!(
        !printed.contains("tstruct.new"),
        "constructor was eagerly transactional"
    );
    ensure!(
        !printed.contains("tarray.new"),
        "constructor was eagerly transactional"
    );

    let temp = tempfile::tempdir()?;
    let tmemory_path = temp.path().join("kotlin-shaped.tmemory");
    let tx_log_path = temp.path().join("kotlin-shaped.txlog");
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, &rewritten)?;
    let mut store = Store::new(&engine, ());
    create_file_backed_storage_for_test(&mut store, tmemory_path, tx_log_path.clone(), 64)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;
    publish.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    ensure!(
        recovered.root_object_ids.len() == 1,
        "expected one recovered Kotlin root, got {:?}",
        recovered.root_object_ids
    );
    ensure!(
        recovered.object_winners.len() >= 3,
        "expected recovered Bank, Account, and Accounts records, got {}",
        recovered.object_winners.len()
    );
    ensure!(
        recovered
            .object_winners
            .iter()
            .any(|winner| winner.object_id == recovered.root_object_ids[0]),
        "root ObjectId was not recovered as an object record"
    );
    let root_winner = recovered
        .object_winners
        .iter()
        .find(|winner| winner.object_id == recovered.root_object_ids[0])
        .expect("root ObjectId should have a recovered object record");
    let alice_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 0)?;
    let mirror_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 1)?;
    ensure!(
        alice_ref == mirror_ref,
        "promotion did not preserve repeated GC-ref aliasing: {alice_ref:?} != {mirror_ref:?}"
    );
    let alice_object_id =
        alice_ref.context("promoted repeated GC-ref alias resolved to null ObjectId")?;
    ensure!(
        recovered
            .object_winners
            .iter()
            .any(|winner| winner.object_id == alice_object_id),
        "aliased Account ObjectId did not have a recovered object record"
    );
    let accounts_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 2)?;
    let accounts_object_id =
        accounts_ref.context("Bank accounts field resolved to null ObjectId")?;
    ensure!(
        recovered
            .object_winners
            .iter()
            .any(|winner| winner.object_id == accounts_object_id),
        "Bank accounts field did not point at a recovered persistent array record"
    );
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

fn recovered_object_i64_field(
    record_bytes: &[u8],
    field_count: usize,
    field_index: usize,
) -> Result<i64> {
    const OBJECT_VALUE_ABI_TAG_I64: u32 = 1;
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
        tag == OBJECT_VALUE_ABI_TAG_I64 && high == 0,
        "expected i64 object field slot, got tag={tag} low={low} high={high}"
    );
    Ok(low as i64)
}

#[derive(Debug, Default)]
struct KotlinTransactionShape {
    tfuncs: usize,
    tfunc_names: BTreeSet<String>,
    tstruct_gets: usize,
    tstruct_sets: usize,
    tarray_gets: usize,
    tarray_sets: usize,
    marker_imports: usize,
}

fn kotlin_transaction_shape(bytes: &[u8]) -> Result<KotlinTransactionShape> {
    let mut shape = KotlinTransactionShape::default();
    let mut tfunc_indices = Vec::new();
    let mut function_names = BTreeMap::<u32, BTreeSet<String>>::new();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.context("failed to parse rewritten Kotlin wasm payload")? {
            Payload::CustomSection(section)
                if section.name() == TRANSACTION_OBJECTS_CUSTOM_SECTION =>
            {
                let mut reader = BinaryReader::new(section.data(), 0);
                let version = reader
                    .read_u8()
                    .context("failed to parse transaction object metadata version")?;
                ensure!(
                    version == 1,
                    "unexpected transaction object metadata version {version}"
                );
                skip_index_set(&mut reader, "memories")?;
                skip_index_set(&mut reader, "globals")?;
                tfunc_indices = read_index_set(&mut reader, "functions")?;
                if !reader.eof() {
                    skip_index_set(&mut reader, "tables")?;
                }
                ensure!(
                    reader.eof(),
                    "transaction object metadata has trailing bytes"
                );
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse rewritten Kotlin import")?;
                    if import.module.starts_with("twasm") {
                        shape.marker_imports += 1;
                    }
                }
            }
            Payload::CustomSection(section) => {
                let KnownCustom::Name(name_section) = section.as_known() else {
                    continue;
                };
                for subsection in name_section {
                    let subsection =
                        subsection.context("failed to parse rewritten Kotlin name subsection")?;
                    let Name::Function(map) = subsection else {
                        continue;
                    };
                    for naming in map {
                        let naming =
                            naming.context("failed to parse rewritten Kotlin function name")?;
                        function_names
                            .entry(naming.index)
                            .or_default()
                            .insert(naming.name.to_string());
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut reader = body
                    .get_operators_reader()
                    .context("failed to read rewritten Kotlin operators")?;
                while !reader.eof() {
                    match reader
                        .read()
                        .context("failed to parse rewritten Kotlin operator")?
                    {
                        Operator::TStructGet { .. }
                        | Operator::TStructGetS { .. }
                        | Operator::TStructGetU { .. } => {
                            shape.tstruct_gets += 1;
                        }
                        Operator::TStructSet { .. } => {
                            shape.tstruct_sets += 1;
                        }
                        Operator::TArrayGet { .. }
                        | Operator::TArrayGetS { .. }
                        | Operator::TArrayGetU { .. }
                        | Operator::TArrayLen => {
                            shape.tarray_gets += 1;
                        }
                        Operator::TArraySet { .. } => {
                            shape.tarray_sets += 1;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    shape.tfuncs = tfunc_indices.len();
    for index in tfunc_indices {
        if let Some(names) = function_names.get(&index) {
            shape.tfunc_names.extend(names.iter().cloned());
        }
    }

    Ok(shape)
}

fn skip_index_set(reader: &mut BinaryReader<'_>, label: &str) -> Result<()> {
    read_index_set(reader, label)?;
    Ok(())
}

fn read_index_set(reader: &mut BinaryReader<'_>, label: &str) -> Result<Vec<u32>> {
    let len = reader
        .read_var_u32()
        .with_context(|| format!("failed to parse transaction object {label} length"))?;
    (0..len)
        .map(|_| {
            reader
                .read_var_u32()
                .with_context(|| format!("failed to parse transaction object {label} index"))
        })
        .collect::<Result<Vec<_>>>()
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
