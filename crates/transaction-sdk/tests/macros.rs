use wasmtime_transaction_sdk::{Persist, Root, txn_func};

#[derive(Persist)]
#[repr(C)]
struct Account {
    balance: i64,
}

#[derive(Persist)]
#[repr(C)]
struct Bank {
    accounts: [Account; 2],
}

#[txn_func]
fn apply_credit(bank: &mut Bank, amount: i64) {
    bank.accounts[0].balance += amount;
}

#[test]
fn derive_persist_uses_rust_layout_values() {
    assert_eq!(Account::TYPE_NAME, "Account");
    assert_eq!(Account::SIZE, core::mem::size_of::<Account>());
    assert_eq!(Bank::ALIGN, core::mem::align_of::<Bank>());
}

#[test]
fn derive_persist_generates_field_metadata() {
    let account_fields = Account::FIELDS;
    assert_eq!(account_fields.len(), 1);
    assert_eq!(account_fields[0].name, "balance");
    assert_eq!(account_fields[0].type_name, i64::TYPE_NAME);
    assert_eq!(
        account_fields[0].offset,
        core::mem::offset_of!(Account, balance)
    );
    assert_eq!(account_fields[0].size, core::mem::size_of::<i64>());
    assert_eq!(account_fields[0].align, core::mem::align_of::<i64>());

    let bank_fields = Bank::FIELDS;
    assert_eq!(bank_fields.len(), 1);
    assert_eq!(bank_fields[0].name, "accounts");
    assert_eq!(bank_fields[0].type_name, <[Account; 2]>::TYPE_NAME);
    assert_eq!(bank_fields[0].offset, core::mem::offset_of!(Bank, accounts));
    assert_eq!(bank_fields[0].size, core::mem::size_of::<[Account; 2]>());
    assert_eq!(bank_fields[0].align, core::mem::align_of::<[Account; 2]>());
}

#[test]
fn root_type_checks_derived_persist_types() {
    let root = Root::<Bank>::tmemory_offset(0);
    assert_eq!(root.id().payload(), 0);
}

#[test]
fn txn_func_expands_for_mut_helpers() {
    let mut bank = Bank {
        accounts: [Account { balance: 5 }, Account { balance: 7 }],
    };

    apply_credit(&mut bank, 4);

    assert_eq!(bank.accounts[0].balance, 9);
    assert_eq!(bank.accounts[1].balance, 7);
}

#[test]
fn derive_persist_frames_multiple_type_names_in_wasm_metadata() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    if !ensure_wasm32_target_or_skip("derive_persist_frames_multiple_type_names_in_wasm_metadata") {
        return;
    }

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_wasm_probe_manifest(manifest_dir, sdk_manifest_dir, "tx-persist-metadata-probe");
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"#![no_std]

use core::panic::PanicInfo;
use sdk::Persist;

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[derive(Persist)]
#[repr(C)]
pub struct Account {
    balance: i64,
}

#[derive(Persist)]
#[repr(C)]
pub struct Ledger {
    account: Account,
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--quiet")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("build probe crate");
    assert!(
        output.status.success(),
        "probe crate failed to build:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let wasm = fs::read(
        manifest_dir.join("target/wasm32-unknown-unknown/debug/tx_persist_metadata_probe.wasm"),
    )
    .expect("read probe wasm");
    let metadata = custom_section_bytes(&wasm, "twasm.persist");
    let mut type_names = parse_persist_metadata_records(&metadata).expect("parse persist metadata");
    type_names.sort();

    assert_eq!(type_names, vec!["Account", "Ledger"]);
}

#[test]
fn txn_func_emits_wasm_markers_for_mut_self_methods() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    if !ensure_wasm32_target_or_skip("txn_func_emits_wasm_markers_for_mut_self_methods") {
        return;
    }

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_wasm_probe_manifest(manifest_dir, sdk_manifest_dir, "tx-macro-probe");
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"#![no_std]

use core::panic::PanicInfo;
use sdk::{Persist, txn_func};

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[derive(Persist)]
#[repr(C)]
pub struct Account {
    balance: i64,
}

impl Account {
    #[txn_func]
    pub fn credit(&mut self, amount: i64) {
        self.balance += amount;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn call_credit(account: &mut Account, amount: i64) {
    account.credit(amount);
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("build")
        .arg("--quiet")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("build probe crate");
    assert!(
        output.status.success(),
        "probe crate failed to build:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let wasm =
        fs::read(manifest_dir.join("target/wasm32-unknown-unknown/debug/tx_macro_probe.wasm"))
            .expect("read probe wasm");
    assert!(
        export_reaches_transaction_markers(&wasm, "call_credit").expect("analyze wasm call graph"),
        "expected a reachable call path from call_credit to include transaction marking and mark_persistent_arg(0)"
    );
}

#[test]
fn txn_func_rejects_non_persist_mut_args() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_host_probe_manifest(manifest_dir, sdk_manifest_dir);
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"use wasmtime_transaction_sdk::txn_func;

struct Transient;

#[txn_func]
fn touch(value: &mut Transient) {
    let _ = value;
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("check probe crate");
    assert!(
        !output.status.success(),
        "probe crate unexpectedly compiled:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Transient"));
    assert!(stderr.contains("Persist"));
}

#[test]
fn derive_persist_rejects_non_persist_fields() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_host_probe_manifest(manifest_dir, sdk_manifest_dir);
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"use wasmtime_transaction_sdk::Persist;

struct Transient;

#[derive(Persist)]
#[repr(C)]
struct Wrapper {
    field: Transient,
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("check probe crate");
    assert!(
        !output.status.success(),
        "probe crate unexpectedly compiled:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Transient"));
    assert!(stderr.contains("Persist"));
}

#[test]
fn derive_persist_rejects_structs_without_stable_repr() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_host_probe_manifest(manifest_dir, sdk_manifest_dir);
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"use wasmtime_transaction_sdk::Persist;

#[derive(Persist)]
struct Wrapper {
    field: u64,
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("check probe crate");
    assert!(
        !output.status.success(),
        "probe crate unexpectedly compiled:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("repr(C)"));
    assert!(stderr.contains("repr(transparent)"));
}

#[test]
fn derive_persist_accepts_repr_c_structs_with_persist_fields() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    write_host_probe_manifest(manifest_dir, sdk_manifest_dir);
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"use wasmtime_transaction_sdk::Persist;

#[derive(Persist)]
#[repr(C)]
struct Inner {
    balance: u64,
}

#[derive(Persist)]
#[repr(C)]
struct Wrapper {
    inner: Inner,
    debit: i64,
}
"#,
    )
    .expect("write probe source");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(manifest_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", manifest_dir.join("target"))
        .output()
        .expect("check probe crate");
    assert!(
        output.status.success(),
        "probe crate failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_host_probe_manifest(manifest_dir: &std::path::Path, sdk_manifest_dir: &std::path::Path) {
    let sdk_path = manifest_path_string(sdk_manifest_dir);
    std::fs::write(
        manifest_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "tx-macro-host-probe"
version = "0.0.0"
edition = "2024"

[dependencies]
wasmtime-transaction-sdk = {{ path = {sdk_path} }}
"#,
        ),
    )
    .expect("write probe manifest");
}

fn write_wasm_probe_manifest(
    manifest_dir: &std::path::Path,
    sdk_manifest_dir: &std::path::Path,
    package_name: &str,
) {
    let sdk_path = manifest_path_string(sdk_manifest_dir);
    std::fs::write(
        manifest_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{package_name}"
version = "0.0.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
sdk = {{ package = "wasmtime-transaction-sdk", path = {sdk_path} }}
"#,
        ),
    )
    .expect("write probe manifest");
}

fn ensure_wasm32_target_or_skip(test_name: &str) -> bool {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let output = std::process::Command::new(rustc)
        .arg("--print")
        .arg("target-libdir")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .output()
        .expect("query wasm32 target availability");
    if output.status.success() {
        return true;
    }

    eprintln!(
        "skipping {test_name}: wasm32-unknown-unknown target unavailable\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn custom_section_bytes(wasm: &[u8], section_name: &str) -> Vec<u8> {
    use wasmparser::{Parser, Payload};

    let mut bytes = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        if let Payload::CustomSection(section) = payload.expect("parse wasm payload") {
            if section.name() == section_name {
                bytes.extend_from_slice(section.data());
            }
        }
    }
    bytes
}

fn parse_persist_metadata_records(bytes: &[u8]) -> Result<Vec<String>, String> {
    const HEADER_LEN: usize = 7;

    let mut offset = 0;
    let mut type_names = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < HEADER_LEN {
            return Err(format!(
                "persist metadata truncated at byte {offset}: expected at least {HEADER_LEN} bytes for header"
            ));
        }
        if &bytes[offset..offset + 4] != b"TPRS" {
            return Err(format!(
                "persist metadata magic mismatch at byte {offset}: {:?}",
                &bytes[offset..offset + 4]
            ));
        }
        let version = bytes[offset + 4];
        if version != 1 {
            return Err(format!(
                "persist metadata version mismatch at byte {}: expected 1, got {}",
                offset + 4,
                version
            ));
        }
        let name_len = u16::from_le_bytes([bytes[offset + 5], bytes[offset + 6]]) as usize;
        offset += HEADER_LEN;
        if bytes.len() - offset < name_len {
            return Err(format!(
                "persist metadata truncated at byte {offset}: expected {name_len} name bytes"
            ));
        }
        let type_name = std::str::from_utf8(&bytes[offset..offset + name_len])
            .map_err(|err| format!("persist metadata name is not valid utf-8: {err}"))?;
        type_names.push(type_name.to_string());
        offset += name_len;
    }

    Ok(type_names)
}

#[derive(Clone, Copy, Debug)]
struct WasmCall {
    callee: u32,
    arg: Option<i32>,
}

fn export_reaches_transaction_markers(wasm: &[u8], export_name: &str) -> Result<bool, String> {
    use std::collections::{HashMap, HashSet};
    use wasmparser::{ExternalKind, Operator, Parser, Payload, TypeRef};

    let mut imported_functions = 0;
    let mut exported_function = None;
    let mut mark_transaction = None;
    let mut mark_persistent = None;
    let mut function_bodies = Vec::new();

    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|err| err.to_string())? {
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    let import = import.map_err(|err| err.to_string())?;
                    if let TypeRef::Func(_) = import.ty {
                        let function_index = imported_functions;
                        imported_functions += 1;
                        match (import.module, import.name) {
                            ("twasm_intrinsics", "__twasm_mark_transaction_func") => {
                                mark_transaction = Some(function_index);
                            }
                            ("twasm_intrinsics", "__twasm_mark_persistent_arg") => {
                                mark_persistent = Some(function_index);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Payload::ExportSection(exports) => {
                for export in exports {
                    let export = export.map_err(|err| err.to_string())?;
                    if export.kind == ExternalKind::Func && export.name == export_name {
                        exported_function = Some(export.index);
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let function_index = imported_functions + function_bodies.len() as u32;
                let mut previous_i32_const = None;
                let mut calls = Vec::new();
                let mut operators = body.get_operators_reader().map_err(|err| err.to_string())?;
                while !operators.eof() {
                    match operators.read().map_err(|err| err.to_string())? {
                        Operator::I32Const { value } => previous_i32_const = Some(value),
                        Operator::Call { function_index } => {
                            calls.push(WasmCall {
                                callee: function_index,
                                arg: previous_i32_const,
                            });
                            previous_i32_const = None;
                        }
                        _ => previous_i32_const = None,
                    }
                }
                function_bodies.push((function_index, calls));
            }
            _ => {}
        }
    }

    let exported_function =
        exported_function.ok_or_else(|| format!("missing export named {export_name}"))?;
    let mark_transaction =
        mark_transaction.ok_or_else(|| "missing transaction marker import".to_string())?;
    let mark_persistent =
        mark_persistent.ok_or_else(|| "missing persistent marker import".to_string())?;

    let calls_by_function = function_bodies.into_iter().collect::<HashMap<_, _>>();
    let mut stack = vec![exported_function];
    let mut seen = HashSet::new();
    let mut saw_transaction = false;
    let mut saw_persistent_arg_zero = false;

    while let Some(function_index) = stack.pop() {
        if !seen.insert(function_index) {
            continue;
        }
        let Some(calls) = calls_by_function.get(&function_index) else {
            continue;
        };
        for call in calls {
            if call.callee == mark_transaction {
                saw_transaction = true;
                continue;
            }
            if call.callee == mark_persistent && call.arg == Some(0) {
                saw_persistent_arg_zero = true;
                continue;
            }
            if call.arg == Some(0)
                && reaches_persistent_import_wrapper(
                    &calls_by_function,
                    call.callee,
                    mark_persistent,
                    &mut HashSet::new(),
                )
            {
                saw_persistent_arg_zero = true;
            }
            if calls_by_function.contains_key(&call.callee) {
                stack.push(call.callee);
            }
        }
    }

    Ok(saw_transaction && saw_persistent_arg_zero)
}

fn reaches_persistent_import_wrapper(
    calls_by_function: &std::collections::HashMap<u32, Vec<WasmCall>>,
    function_index: u32,
    mark_persistent: u32,
    seen: &mut std::collections::HashSet<u32>,
) -> bool {
    if !seen.insert(function_index) {
        return false;
    }
    let Some(calls) = calls_by_function.get(&function_index) else {
        return false;
    };
    calls.iter().any(|call| {
        call.callee == mark_persistent
            || reaches_persistent_import_wrapper(
                calls_by_function,
                call.callee,
                mark_persistent,
                seen,
            )
    })
}

fn manifest_path_string(path: &std::path::Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}
