use wasmtime_transaction_tools::kotlin_inspect::inspect_wasm_shape;

#[test]
fn shape_report_counts_wasmgc_struct_and_array_ops() {
    let wat = r#"
        (module
          (import "twasm.marker" "transaction" (func))
          (type $account (struct (field (mut i64))))
          (type $accounts (array (mut (ref null $account))))
          (func (export "make") (result (ref $account))
            (struct.new $account (i64.const 1)))
          (func (export "read") (param (ref $account)) (result i64)
            (struct.get $account 0 (local.get 0)))
          (func (export "write") (param (ref $account))
            (struct.set $account 0 (local.get 0) (i64.const 2)))
          (func (export "array_make") (result (ref $accounts))
            (array.new $accounts (ref.null $account) (i32.const 2)))
          (func (export "array_read") (param (ref $accounts)) (result (ref null $account))
            (array.get $accounts (local.get 0) (i32.const 0)))
          (func (export "array_write") (param (ref $accounts))
            (array.set $accounts (local.get 0) (i32.const 0) (ref.null $account)))
          (func (export "array_len") (param (ref $accounts)) (result i32)
            (array.len (local.get 0))))
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let report = inspect_wasm_shape(&wasm).unwrap();

    assert_eq!(report.functions, 7);
    assert_eq!(report.gc_struct_types, 1);
    assert_eq!(report.gc_array_types, 1);
    assert_eq!(report.struct_new_ops, 1);
    assert_eq!(report.array_new_ops, 1);
    assert_eq!(report.struct_get_ops, 1);
    assert_eq!(report.struct_set_ops, 1);
    assert_eq!(report.array_get_ops, 1);
    assert_eq!(report.array_set_ops, 1);
    assert_eq!(report.array_len_ops, 1);
    assert_eq!(report.marker_imports, 1);
}
