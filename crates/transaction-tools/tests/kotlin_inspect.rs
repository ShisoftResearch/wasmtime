use std::fs;
use std::process::Command;

#[test]
fn twasm_kotlin_inspect_reports_sidecar_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let sidecar = temp.path().join("twasm.kotlin.json");
    fs::write(
        &sidecar,
        r#"{
          "version": 1,
          "module": "fixture",
          "persistentTypes": [
            { "name": "Account", "kind": "struct", "fields": [
              { "name": "balance", "kind": "i64", "nullable": false }
            ] }
          ],
          "transactionFunctions": ["transfer"],
          "roots": [
            { "name": "bank", "type": "Account", "nullable": false }
          ]
        }"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_twasm-kotlin"))
        .args(["inspect", "--metadata"])
        .arg(&sidecar)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["module"], "fixture");
    assert_eq!(report["persistent_types"], 1);
    assert_eq!(report["transaction_functions"], 1);
    assert_eq!(report["roots"], 1);
}
