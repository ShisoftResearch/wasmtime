use wasmparser::{BinaryReader, Parser, Payload, Validator};
use wasmtime_transaction_tools::kotlin_metadata::{
    KotlinField, KotlinFieldKind, KotlinPersistentKind, KotlinPersistentType, KotlinRoot,
    KotlinSidecar,
};
use wasmtime_transaction_tools::kotlin_rewrite::rewrite_kotlin_module;

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";

fn sidecar(
    persistent_types: Vec<KotlinPersistentType>,
    transaction_functions: &[&str],
    roots: Vec<KotlinRoot>,
) -> KotlinSidecar {
    KotlinSidecar {
        version: 1,
        module: "fixture".into(),
        persistent_types,
        transaction_functions: transaction_functions
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        roots,
    }
}

fn struct_type(name: &str) -> KotlinPersistentType {
    KotlinPersistentType {
        name: name.into(),
        kind: KotlinPersistentKind::Struct,
        fields: vec![KotlinField {
            name: "balance".into(),
            kind: KotlinFieldKind::I64,
            r#type: None,
            nullable: false,
        }],
        element: None,
    }
}

fn array_type(name: &str) -> KotlinPersistentType {
    KotlinPersistentType {
        name: name.into(),
        kind: KotlinPersistentKind::Array,
        fields: vec![],
        element: Some(KotlinField {
            name: "element".into(),
            kind: KotlinFieldKind::I32,
            r#type: None,
            nullable: false,
        }),
    }
}

#[test]
fn rewrite_report_rejects_missing_transaction_function() {
    let input = wat::parse_str(
        r#"
            (module
              (func (export "not_transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("transaction function transfer was not found"));
}

#[test]
fn rewrite_report_records_transaction_function_names() {
    let input = wat::parse_str(
        r#"
            (module
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();

    assert_eq!(report.transaction_functions, vec!["transfer"]);
    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(transaction_function_metadata(&output), vec![0]);
}

#[test]
fn rewrite_lowers_struct_object_ops_in_marked_transaction_function() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (func (export "transfer") (param $account (ref $account)) (param $delta i64) (result i64)
                local.get $account
                struct.get $account 0
                local.get $delta
                i64.add
                local.get $account
                local.get $account
                struct.get $account 0
                local.get $delta
                i64.add
                struct.set $account 0))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert!(report.rewritten_object_ops > 0, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("tref.cast_read"), "{printed}");
    assert!(printed.contains("tref.cast_write"), "{printed}");
    assert!(printed.contains("tstruct.get"), "{printed}");
    assert!(printed.contains("tstruct.set"), "{printed}");
}

#[test]
fn rewrite_lowers_array_object_ops_in_marked_transaction_function() {
    let input = wat::parse_str(
        r#"
            (module
              (type $accounts (array (mut i32)))
              (func (export "transfer") (param $accounts (ref $accounts)) (result i32)
                local.get $accounts
                i32.const 0
                array.get $accounts
                drop
                local.get $accounts
                i32.const 0
                i32.const 7
                array.set $accounts
                local.get $accounts
                array.len))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Accounts")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert!(report.rewritten_object_ops > 0, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("tref.cast_read"), "{printed}");
    assert!(printed.contains("tref.cast_write"), "{printed}");
    assert!(printed.contains("tarray.get"), "{printed}");
    assert!(printed.contains("tarray.set"), "{printed}");
    assert!(printed.contains("tarray.len"), "{printed}");
}

#[test]
fn rewrite_keeps_array_len_for_unmapped_array_type() {
    let input = wat::parse_str(
        r#"
            (module
              (type $persistent (array (mut i32)))
              (type $ordinary (array (mut i32)))
              (func (export "transfer") (param $ordinary (ref $ordinary)) (result i32)
                local.get $ordinary
                array.len))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Persistent")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_object_ops, 0, "{report:?}");
    assert!(printed.contains("array.len"), "{printed}");
    assert!(!printed.contains("tarray.len"), "{printed}");
}

#[test]
fn rewrite_keeps_array_len_after_persistent_array_set_stack_consumption() {
    let input = wat::parse_str(
        r#"
            (module
              (type $persistent (array (mut i32)))
              (type $ordinary (array (mut i32)))
              (func (export "transfer")
                    (param $persistent (ref $persistent))
                    (param $ordinary (ref $ordinary))
                    (result i32)
                local.get $persistent
                i32.const 0
                i32.const 7
                array.set $persistent
                local.get $ordinary
                array.len))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Persistent")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_object_ops, 1, "{report:?}");
    assert!(printed.contains("tarray.set"), "{printed}");
    assert!(printed.contains("array.len"), "{printed}");
    assert!(!printed.contains("tarray.len"), "{printed}");
}

#[test]
fn rewrite_does_not_use_stale_stack_types_after_call() {
    let input = wat::parse_str(
        r#"
            (module
              (type $persistent (array (mut i32)))
              (type $ordinary (array (mut i32)))
              (type $make_ty (func (param (ref $persistent)) (result (ref $ordinary))))
              (import "fixture" "make_ordinary" (func $make_ordinary (type $make_ty)))
              (func (export "transfer") (param $persistent (ref $persistent)) (result i32)
                local.get $persistent
                call $make_ordinary
                array.len))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Persistent")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_object_ops, 0, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("array.len"), "{printed}");
    assert!(!printed.contains("tarray.len"), "{printed}");
}

#[test]
fn rewrite_preserves_array_len_result_on_type_stack() {
    let input = wat::parse_str(
        r#"
            (module
              (type $persistent (array (mut i32)))
              (type $ordinary (array (mut i32)))
              (func (export "transfer")
                    (param $persistent (ref $persistent))
                    (param $ordinary (ref $ordinary))
                    (result i32)
                local.get $persistent
                local.get $ordinary
                array.len
                drop
                array.len))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Persistent")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_object_ops, 1, "{report:?}");
    assert!(printed.contains("tarray.len"), "{printed}");
}

fn transaction_function_metadata(bytes: &[u8]) -> Vec<u32> {
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("payload");
        let Payload::CustomSection(section) = payload else {
            continue;
        };
        if section.name() != TRANSACTION_OBJECTS_CUSTOM_SECTION {
            continue;
        }

        let mut reader = BinaryReader::new(section.data(), 0);
        assert_eq!(reader.read_u8().expect("version"), 1);
        assert!(read_index_vec(&mut reader).is_empty(), "memories");
        assert!(read_index_vec(&mut reader).is_empty(), "globals");
        let functions = read_index_vec(&mut reader);
        assert!(read_index_vec(&mut reader).is_empty(), "tables");
        assert!(reader.eof(), "metadata trailing bytes");
        return functions;
    }

    Vec::new()
}

fn assert_valid_module(bytes: &[u8]) {
    Validator::new()
        .validate_all(bytes)
        .expect("rewritten module should validate");
}

fn read_index_vec(reader: &mut BinaryReader<'_>) -> Vec<u32> {
    let len = reader.read_var_u32().expect("vector length");
    (0..len)
        .map(|_| reader.read_var_u32().expect("vector item"))
        .collect()
}
