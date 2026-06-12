use wasm_encoder::{CustomSection, Module};
use wasmtime_transaction_tools::cli::{Cli, run};
use wasmtime_transaction_tools::metadata::inspect_module;

use clap::Parser;

fn persist_record(name: &str) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let mut bytes = Vec::with_capacity(7 + name_bytes.len());
    bytes.extend_from_slice(b"TPRS");
    bytes.push(1);
    bytes.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    bytes.extend_from_slice(name_bytes);
    bytes
}

#[test]
fn inspect_finds_framed_persist_metadata_records() {
    let mut first_section = persist_record("Account");
    first_section.extend_from_slice(&persist_record("Bank"));

    let mut module = Module::new();
    module.section(&CustomSection {
        name: "twasm.persist".into(),
        data: first_section.as_slice().into(),
    });
    module.section(&CustomSection {
        name: "twasm.persist".into(),
        data: persist_record("Ledger").as_slice().into(),
    });

    let report = inspect_module(&module.finish()).unwrap();
    assert_eq!(report.persist_sections, 2);
    assert_eq!(report.persist_payloads, vec!["Account", "Bank", "Ledger"]);
}

#[test]
fn inspect_cli_writes_metadata_json() {
    let mut module = Module::new();
    module.section(&CustomSection {
        name: "twasm.persist".into(),
        data: persist_record("Bank").as_slice().into(),
    });

    let tempdir = tempfile::tempdir().unwrap();
    let input = tempdir.path().join("bank.wasm");
    std::fs::write(&input, module.finish()).unwrap();

    let cli = Cli::parse_from(["twasm-rust", "inspect", input.to_str().unwrap()]);
    let mut stdout = Vec::new();
    run(cli, &mut stdout).unwrap();

    let json: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(json["persist_sections"], 1);
    assert_eq!(json["persist_payloads"], serde_json::json!(["Bank"]));
}

#[test]
fn rewrite_cli_documents_noop_output_and_report() {
    let input_bytes = b"\0asm\x01\0\0\0";
    let tempdir = tempfile::tempdir().unwrap();
    let input = tempdir.path().join("input.wasm");
    let output = tempdir.path().join("output.wasm");
    let report = tempdir.path().join("report.json");
    std::fs::write(&input, input_bytes).unwrap();

    let cli = Cli::parse_from([
        "twasm-rust",
        "rewrite",
        input.to_str().unwrap(),
        "--output",
        output.to_str().unwrap(),
        "--report",
        report.to_str().unwrap(),
    ]);
    let mut stdout = Vec::new();
    run(cli, &mut stdout).unwrap();

    assert_eq!(std::fs::read(&output).unwrap(), input_bytes);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(json["transaction_functions"], 0);
    assert_eq!(json["persistent_addr_markers"], 0);
    assert_eq!(json["i32_tloads"], 0);
    assert_eq!(json["i32_tstores"], 0);
}
