use wasm_encoder::{CustomSection, Module};
use wasmtime_transaction_tools::metadata::inspect_module;

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
