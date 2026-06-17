use wasmparser::{BinaryReader, Parser, Payload, Validator};
use wasmtime_transaction_tools::kotlin_metadata::{
    KotlinField, KotlinFieldKind, KotlinPersistentKind, KotlinPersistentType, KotlinRoot,
    KotlinSidecar,
};
use wasmtime_transaction_tools::kotlin_rewrite::rewrite_kotlin_module;

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";

#[derive(Default, Debug, Eq, PartialEq)]
struct TransactionObjects {
    memories: Vec<u32>,
    globals: Vec<u32>,
    functions: Vec<u32>,
    tables: Vec<u32>,
}

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
    struct_type_with_fields(
        name,
        vec![KotlinField {
            name: "balance".into(),
            kind: KotlinFieldKind::I64,
            r#type: None,
            nullable: false,
        }],
    )
}

fn struct_type_with_fields(name: &str, fields: Vec<KotlinField>) -> KotlinPersistentType {
    KotlinPersistentType {
        name: name.into(),
        kind: KotlinPersistentKind::Struct,
        fields,
        element: None,
    }
}

fn array_type(name: &str) -> KotlinPersistentType {
    array_type_with_element(
        name,
        KotlinField {
            name: "element".into(),
            kind: KotlinFieldKind::I32,
            r#type: None,
            nullable: false,
        },
    )
}

fn array_type_with_element(name: &str, element: KotlinField) -> KotlinPersistentType {
    KotlinPersistentType {
        name: name.into(),
        kind: KotlinPersistentKind::Array,
        fields: vec![],
        element: Some(element),
    }
}

fn scalar_field(name: &str, kind: KotlinFieldKind) -> KotlinField {
    KotlinField {
        name: name.into(),
        kind,
        r#type: None,
        nullable: false,
    }
}

fn ref_field(name: &str, target: &str, nullable: bool) -> KotlinField {
    KotlinField {
        name: name.into(),
        kind: KotlinFieldKind::Ref,
        r#type: Some(target.into()),
        nullable,
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
    assert_eq!(transaction_objects(&output).functions, vec![0]);
}

#[test]
fn rewrite_report_records_transaction_function_name_section_matches() {
    let input = wat::parse_str(
        r#"
            (module
              (func $transfer))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();

    assert_eq!(report.transaction_functions, vec!["transfer"]);
    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(transaction_objects(&output).functions, vec![0]);
}

#[test]
fn rewrite_lowers_inline_root_markers_for_distinct_root_types() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (type $Vault (struct))
              (tag $error)
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit)
              (func $installBank (param $bank (ref null $Bank)) (result (ref null $Unit))
                block (result (ref null $Unit))
                  local.get $bank
                  local.set $bank
                  i32.const 658
                  drop
                  block
                    throw $error
                  end
                  unreachable
                end)
              (func $readVault (result (ref null $Vault))
                block (result (ref null $Vault))
                  i32.const 659
                  drop
                  block
                    throw $error
                  end
                  unreachable
                end))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![
            struct_type_with_fields("Bank", vec![]),
            struct_type_with_fields("Vault", vec![]),
        ],
        &[],
        vec![
            KotlinRoot {
                name: "bank".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
            KotlinRoot {
                name: "vault".into(),
                r#type: "Vault".into(),
                nullable: true,
            },
        ],
    );

    let (output, _) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();
    assert_valid_module(&output);

    assert_eq!(
        transaction_objects(&output),
        TransactionObjects {
            memories: vec![],
            globals: vec![0, 1],
            functions: vec![1, 2],
            tables: vec![],
        }
    );
    assert!(printed.contains("tglobal.set 0"), "{printed}");
    assert!(printed.contains("tglobal.get 1"), "{printed}");
}

#[test]
fn rewrite_rejects_ambiguous_inline_root_marker_type() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (tag $error)
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit)
              (func $readBank (result (ref null $Bank))
                block (result (ref null $Bank))
                  i32.const 659
                  drop
                  block
                    throw $error
                  end
                  unreachable
                end))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![
            KotlinRoot {
                name: "primary".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
            KotlinRoot {
                name: "secondary".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
        ],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("ambiguous Kotlin root marker"), "{err}");
}

#[test]
fn rewrite_lowers_same_type_root_marker_imports() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (import "twasm.root.set" "primary"
                (func $set_primary (param (ref null $Bank)) (result (ref null $Unit))))
              (import "twasm.root.get" "secondary"
                (func $get_secondary (result (ref null $Bank))))
              (import "env" "observe"
                (func $observe (param (ref null $Bank))))
              (tag $error)
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit)
              (func $installPrimary (param $bank (ref null $Bank)) (result (ref null $Unit))
                local.get $bank
                call $set_primary)
              (func $readSecondary (result (ref null $Bank))
                call $get_secondary)
              (func $passToEnv (param $bank (ref null $Bank))
                local.get $bank
                call $observe))
            "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![
            KotlinRoot {
                name: "primary".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
            KotlinRoot {
                name: "secondary".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
        ],
    );

    let (output, _) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();
    assert_valid_module(&output);
    assert_eq!(marker_import_count(&output), 0);

    assert_eq!(
        transaction_objects(&output),
        TransactionObjects {
            memories: vec![],
            globals: vec![0, 1],
            functions: vec![2, 3],
            tables: vec![],
        }
    );
    assert!(printed.contains("tglobal.set 0"), "{printed}");
    assert!(printed.contains("tglobal.get 1"), "{printed}");
}

#[test]
fn rewrite_lowers_inline_and_explicit_root_markers_together() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (type $Vault (struct))
              (import "twasm.root.get" "bank"
                (func $get_bank (result (ref null $Bank))))
              (tag $error)
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit)
              (func $installVault (param $vault (ref null $Vault)) (result (ref null $Unit))
                block (result (ref null $Unit))
                  local.get $vault
                  local.set $vault
                  i32.const 658
                  drop
                  block
                    throw $error
                  end
                  unreachable
                end)
              (func $readBank (result (ref null $Bank))
                call $get_bank))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![
            struct_type_with_fields("Bank", vec![]),
            struct_type_with_fields("Vault", vec![]),
        ],
        &[],
        vec![
            KotlinRoot {
                name: "bank".into(),
                r#type: "Bank".into(),
                nullable: true,
            },
            KotlinRoot {
                name: "vault".into(),
                r#type: "Vault".into(),
                nullable: true,
            },
        ],
    );

    let (output, _) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();
    assert_valid_module(&output);
    assert_eq!(marker_import_count(&output), 0);

    assert_eq!(
        transaction_objects(&output),
        TransactionObjects {
            memories: vec![],
            globals: vec![0, 1],
            functions: vec![1, 2],
            tables: vec![],
        }
    );
    assert!(printed.contains("tglobal.get 0"), "{printed}");
    assert!(printed.contains("tglobal.set 1"), "{printed}");
}

#[test]
fn rewrite_rejects_unknown_root_marker_import_name() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Bank (struct))
              (import "twasm.root.get" "missing"
                (func $get_missing (result (ref null $Bank)))))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("names unknown root"), "{err}");
}

#[test]
fn rewrite_rejects_root_marker_import_type_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Bank (struct))
              (type $Vault (struct))
              (import "twasm.root.get" "bank"
                (func $get_bank (result (ref null $Vault)))))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("has type index"), "{err}");
}

#[test]
fn rewrite_rejects_root_marker_import_nullability_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Bank (struct))
              (import "twasm.root.get" "bank"
                (func $get_bank (result (ref $Bank)))))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("must use nullable concrete root ref"), "{err}");
}

#[test]
fn rewrite_rejects_root_marker_import_exact_ref() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Bank (struct))
              (import "twasm.root.get" "bank"
                (func $get_bank (result (ref (exact $Bank))))))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("must use nullable concrete root ref"), "{err}");
}

#[test]
fn rewrite_rejects_root_marker_import_abstract_ref() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Bank (struct))
              (import "twasm.root.get" "bank"
                (func $get_bank (result (ref null struct)))))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("must use nullable concrete root ref"), "{err}");
}

#[test]
fn rewrite_rejects_root_set_marker_param_type_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (type $Vault (struct))
              (import "twasm.root.set" "bank"
                (func $set_bank (param (ref null $Vault)) (result (ref null $Unit))))
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("has type index"), "{err}");
}

#[test]
fn rewrite_rejects_root_set_marker_wrong_result_type() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Unit (struct))
              (type $Bank (struct))
              (import "twasm.root.set" "bank"
                (func $set_bank (param (ref null $Bank)) (result (ref null $Bank))))
              (func (export "kotlin.Unit_getInstance") (result (ref null $Unit))
                ref.null $Unit))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields("Bank", vec![])],
        &[],
        vec![KotlinRoot {
            name: "bank".into(),
            r#type: "Bank".into(),
            nullable: true,
        }],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("must return kotlin.Unit_getInstance type"),
        "{err}"
    );
}

#[test]
fn rewrite_merges_existing_transaction_object_metadata() {
    let input = wat::parse_str(
        r#"
            (module
              (tglobal $root (mut i32) (i32.const 0))
              (func (export "publish")
                i32.const 1
                tglobal.set $root))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![], &["publish"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(
        transaction_objects(&output),
        TransactionObjects {
            memories: vec![],
            globals: vec![0],
            functions: vec![0],
            tables: vec![],
        }
    );
}

#[test]
fn rewrite_merges_existing_transaction_table_metadata() {
    let input = wat::parse_str(
        r#"
            (module
              (ttable $root 1 funcref)
              (func (export "publish")
                ttable.size $root
                drop))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![], &["publish"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(
        transaction_objects(&output),
        TransactionObjects {
            memories: vec![],
            globals: vec![],
            functions: vec![0],
            tables: vec![0],
        }
    );
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
fn rewrite_keeps_persistent_constructors_ordinary_until_promotion_boundary() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (type $accounts (array (mut i32)))
              (func (export "transfer")
                i64.const 10
                struct.new $account
                drop
                i32.const 7
                i32.const 2
                array.new $accounts
                drop))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type("Account"), array_type("Accounts")],
        &["transfer"],
        vec![],
    );

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_object_ops, 0, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("struct.new"), "{printed}");
    assert!(printed.contains("array.new"), "{printed}");
    assert!(!printed.contains("tstruct.new"), "{printed}");
    assert!(!printed.contains("tarray.new"), "{printed}");
}

#[test]
fn rewrite_maps_persistent_gc_types_by_name_section_before_ordinal() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Runtime (struct (field (mut i32))))
              (type $Account (struct (field (mut i64))))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 0))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(report.rewritten_object_ops, 1, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("tstruct.get"), "{printed}");
}

#[test]
fn rewrite_validates_named_persistent_fields_after_runtime_prefix() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account
                (struct
                  (field $vtable (mut i32))
                  (field $itable (mut i32))
                  (field $rtti (mut i32))
                  (field $_hashCode (mut i32))
                  (field $balance (mut i64))))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 4))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(report.rewritten_object_ops, 1, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("tstruct.get"), "{printed}");
}

#[test]
fn rewrite_rejects_named_struct_sidecar_missing_persistent_field() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account
                (struct
                  (field $vtable (mut i32))
                  (field $itable (mut i32))
                  (field $rtti (mut i32))
                  (field $_hashCode (mut i32))
                  (field $balance (mut i64))
                  (field $status (mut i32))))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 4))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("missing persistent field status"),
        "unexpected error: {err}"
    );
}

#[test]
fn rewrite_rejects_partial_named_struct_fields_with_unnamed_persistent_field() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account
                (struct
                  (field $vtable (mut i32))
                  (field $itable (mut i32))
                  (field $rtti (mut i32))
                  (field $_hashCode (mut i32))
                  (field $balance (mut i64))
                  (field (mut i32))))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 4))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("has unnamed non-runtime WasmGC fields"),
        "unexpected error: {err}"
    );
}

#[test]
fn rewrite_lowers_persistent_accessor_bodies_without_marking_tfuncs() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account (struct (field $balance (mut i64))))
              (func $get (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 0)
              (export "Account.<get-balance>" (func $get))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                call $get))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();
    let objects = transaction_objects(&output);

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(report.rewritten_object_ops, 1, "{report:?}");
    assert_eq!(objects.functions, vec![1]);
    assert_valid_module(&output);
    assert!(printed.contains("tstruct.get"), "{printed}");
}

#[test]
fn rewrite_rejects_ambiguous_persistent_accessor_name_matches() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account (struct (field $balance (mut i64))))
              (func $Account.<get-balance> (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 0)
              (func $other_get (param $account (ref $Account)) (result i64)
                local.get $account
                struct.get $Account 0)
              (export "Account.<get-balance>" (func $other_get))
              (func $transfer (export "transfer") (param $account (ref $Account)) (result i64)
                local.get $account
                call $Account.<get-balance>))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("ambiguous Kotlin rewrite persistent accessor Account.<get-balance>"),
        "unexpected error: {err}"
    );
}

#[test]
fn rewrite_falls_back_to_next_unmatched_gc_type_after_named_type_mapping() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Runtime (struct (field (mut i32))))
              (type $Account (struct (field (mut i64))))
              (type (struct (field (mut (ref null $Account)))))
              (func $transfer (export "transfer") (param $bank (ref 2)) (result i64)
                local.get $bank
                struct.get 2 0
                ref.as_non_null
                struct.get $Account 0))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![
            struct_type("Account"),
            struct_type_with_fields("Bank", vec![ref_field("account", "Account", true)]),
        ],
        &["transfer"],
        vec![],
    );

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.rewritten_tfuncs, 1);
    assert_eq!(report.rewritten_object_ops, 2, "{report:?}");
    assert_valid_module(&output);
    assert!(printed.contains("tstruct.get $Account 0"), "{printed}");
    assert!(printed.contains("tstruct.get 2 0"), "{printed}");
}

#[test]
fn rewrite_rejects_duplicate_sidecar_type_index_mapping() {
    let input = wat::parse_str(
        r#"
            (module
              (type $Account (struct (field (mut i64))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type("Account"), struct_type("Bank")],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("persistent type Bank could not be mapped"),
        "unexpected error: {err}"
    );
}

#[test]
fn rewrite_rejects_sidecar_struct_field_count_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields(
            "Account",
            vec![
                scalar_field("balance", KotlinFieldKind::I64),
                scalar_field("status", KotlinFieldKind::I32),
            ],
        )],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("field count mismatch"), "{err}");
}

#[test]
fn rewrite_rejects_sidecar_struct_field_type_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i32))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![struct_type("Account")], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("field balance type mismatch"), "{err}");
}

#[test]
fn rewrite_rejects_sidecar_packed_struct_field_as_i32() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i8))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields(
            "Account",
            vec![scalar_field("balance", KotlinFieldKind::I32)],
        )],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("expected i32, found i8"), "{err}");
}

#[test]
fn rewrite_rejects_missing_sidecar_layout_metadata() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type("Account"), struct_type("Missing")],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("persistent type Missing could not be mapped"),
        "{err}"
    );
}

#[test]
fn rewrite_rejects_persistent_ref_to_non_persistent_gc_type() {
    let input = wat::parse_str(
        r#"
            (module
              (type $holder (struct (field (mut (ref null $ordinary)))))
              (type $ordinary (struct (field (mut i64))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![struct_type_with_fields(
            "Holder",
            vec![ref_field("child", "Holder", true)],
        )],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("points at non-persistent GC type"), "{err}");
}

#[test]
fn rewrite_rejects_sidecar_array_element_type_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $accounts (array (mut i64)))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(vec![array_type("Accounts")], &["transfer"], vec![]);

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("persistent type Accounts element type mismatch"),
        "{err}"
    );
}

#[test]
fn rewrite_rejects_sidecar_ref_nullability_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (type $holder (struct (field (mut (ref null $account)))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![
            struct_type("Account"),
            struct_type_with_fields("Holder", vec![ref_field("child", "Account", false)]),
        ],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("nullability mismatch"), "{err}");
}

#[test]
fn rewrite_rejects_sidecar_ref_target_mismatch() {
    let input = wat::parse_str(
        r#"
            (module
              (type $account (struct (field (mut i64))))
              (type $profile (struct (field (mut i64))))
              (type $holder (struct (field (mut (ref null $profile)))))
              (func (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = sidecar(
        vec![
            struct_type("Account"),
            struct_type_with_fields("Profile", vec![scalar_field("id", KotlinFieldKind::I64)]),
            struct_type_with_fields("Holder", vec![ref_field("child", "Account", true)]),
        ],
        &["transfer"],
        vec![],
    );

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(err.contains("expected ref to Account"), "{err}");
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

fn transaction_objects(bytes: &[u8]) -> TransactionObjects {
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
        let memories = read_index_vec(&mut reader);
        let globals = read_index_vec(&mut reader);
        let functions = read_index_vec(&mut reader);
        let tables = read_index_vec(&mut reader);
        assert!(reader.eof(), "metadata trailing bytes");
        return TransactionObjects {
            memories,
            globals,
            functions,
            tables,
        };
    }

    TransactionObjects::default()
}

fn assert_valid_module(bytes: &[u8]) {
    Validator::new()
        .validate_all(bytes)
        .expect("rewritten module should validate");
}

fn marker_import_count(bytes: &[u8]) -> usize {
    let mut count = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("payload");
        let Payload::ImportSection(section) = payload else {
            continue;
        };
        for import in section.into_imports() {
            let import = import.expect("import");
            if matches!(import.module, "twasm.root.get" | "twasm.root.set") {
                count += 1;
            }
        }
    }
    count
}

fn read_index_vec(reader: &mut BinaryReader<'_>) -> Vec<u32> {
    let len = reader.read_var_u32().expect("vector length");
    (0..len)
        .map(|_| reader.read_var_u32().expect("vector item"))
        .collect()
}
