use wasmtime_transaction_sdk::{Persist, Root, transaction_attr};

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

#[transaction_attr]
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
fn root_type_checks_derived_persist_types() {
    let root = Root::<Bank>::tmemory_offset(0);
    assert_eq!(root.id().payload(), 0);
}

#[test]
fn transaction_attr_expands_for_mut_helpers() {
    let mut bank = Bank {
        accounts: [Account { balance: 5 }, Account { balance: 7 }],
    };

    apply_credit(&mut bank, 4);

    assert_eq!(bank.accounts[0].balance, 9);
    assert_eq!(bank.accounts[1].balance, 7);
}

#[test]
fn transaction_attr_emits_wasm_markers_for_mut_self_methods() {
    use std::fs;
    use std::process::Command;

    use tempfile::tempdir;
    use wasmparser::{Operator, Parser, Payload, TypeRef};

    let tempdir = tempdir().expect("create temp dir");
    let manifest_dir = tempdir.path();
    let sdk_manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let sdk_path = manifest_path_string(sdk_manifest_dir);
    fs::write(
        manifest_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "tx-macro-probe"
version = "0.0.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
sdk = {{ package = "wasmtime-transaction-sdk", path = {} }}
"#,
            sdk_path
        ),
    )
    .expect("write probe manifest");
    fs::create_dir(manifest_dir.join("src")).expect("create src dir");
    fs::write(
        manifest_dir.join("src/lib.rs"),
        r#"#![no_std]

use core::panic::PanicInfo;
use sdk::{Persist, transaction_attr};

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
    #[transaction_attr]
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

    let mut imported_functions = 0;
    let mut mark_transaction = None;
    let mut mark_persistent = None;
    let mut function_bodies = Vec::new();

    for payload in Parser::new(0).parse_all(&wasm) {
        match payload.expect("parse wasm payload") {
            Payload::ImportSection(imports) => {
                for import in imports.into_imports() {
                    let import = import.expect("read wasm import");
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
            Payload::CodeSectionEntry(body) => {
                let function_index = imported_functions + function_bodies.len() as u32;
                let mut previous_i32_const = None;
                let mut calls = Vec::new();
                let mut operators = body.get_operators_reader().expect("read operators");
                while !operators.eof() {
                    match operators.read().expect("read operator") {
                        Operator::I32Const { value } => previous_i32_const = Some(value),
                        Operator::Call { function_index } => {
                            calls.push((function_index, previous_i32_const));
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

    assert!(
        mark_transaction.is_some(),
        "missing transaction marker import"
    );
    assert!(
        mark_persistent.is_some(),
        "missing persistent marker import"
    );
    let mark_transaction = mark_transaction.expect("transaction import index");
    let mark_persistent = mark_persistent.expect("persistent import index");
    let transaction_wrapper = function_bodies
        .iter()
        .find_map(|(function_index, calls)| {
            calls
                .iter()
                .any(|(callee, _)| *callee == mark_transaction)
                .then_some(*function_index)
        })
        .expect("missing transaction marker wrapper");
    let persistent_wrappers = function_bodies
        .iter()
        .filter_map(|(function_index, calls)| {
            calls
                .iter()
                .any(|(callee, _)| *callee == mark_persistent)
                .then_some(*function_index)
        })
        .collect::<Vec<_>>();
    let saw_transaction_and_receiver_marker = function_bodies.iter().any(|(_, calls)| {
        let saw_transaction_wrapper = calls
            .iter()
            .any(|(callee, _)| *callee == transaction_wrapper);
        let saw_receiver_marker = calls
            .iter()
            .any(|(callee, arg)| persistent_wrappers.contains(callee) && *arg == Some(0));
        saw_transaction_wrapper && saw_receiver_marker
    });
    assert!(
        saw_transaction_and_receiver_marker,
        "expected a function body to call both transaction marking and mark_persistent_arg(0)"
    );
}

#[test]
fn transaction_attr_rejects_non_persist_mut_args() {
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
        r#"use wasmtime_transaction_sdk::transaction_attr;

struct Transient;

#[transaction_attr]
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
wasmtime-transaction-sdk = {{ path = {} }}
"#,
            sdk_path
        ),
    )
    .expect("write probe manifest");
}

fn manifest_path_string(path: &std::path::Path) -> String {
    toml::Value::String(path.to_string_lossy().into_owned()).to_string()
}
