use wasmtime_transaction_tools::{kotlin, rust};

#[test]
fn exposes_language_specific_tool_namespaces() {
    let sidecar = kotlin::metadata::parse_kotlin_sidecar(
        br#"{
            "version": 1,
            "module": "layout",
            "persistentTypes": [
                { "name": "Root", "kind": "struct", "fields": [] }
            ],
            "transactionFunctions": [],
            "roots": [
                { "name": "root", "type": "Root", "nullable": true }
            ]
        }"#
        .as_slice(),
    )
    .unwrap();
    assert_eq!(sidecar.module, "layout");

    let _ = kotlin::cli::main as fn() -> anyhow::Result<()>;
    let _ = kotlin::inspect::inspect_wasm_shape
        as fn(&[u8]) -> anyhow::Result<kotlin::inspect::KotlinWasmShapeReport>;
    let _ = kotlin::rewrite::rewrite_kotlin_module
        as fn(
            &[u8],
            &kotlin::metadata::KotlinSidecar,
        ) -> anyhow::Result<(Vec<u8>, kotlin::rewrite::KotlinRewriteReport)>;

    let _ = rust::cli::main as fn() -> anyhow::Result<()>;
    let _ = rust::metadata::inspect_module
        as fn(&[u8]) -> anyhow::Result<rust::metadata::MetadataReport>;
    let _ = rust::rewrite::rewrite_module
        as fn(&[u8]) -> anyhow::Result<(Vec<u8>, rust::rewrite::RewriteReport)>;
}
