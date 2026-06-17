use wasmtime_transaction_tools::kotlin_metadata::{
    KotlinFieldKind, KotlinPersistentKind, parse_kotlin_sidecar,
};

#[test]
fn parses_bank_sidecar() {
    let json = br#"{
        "version": 1,
        "module": "transaction-kotlin-bank",
        "persistentTypes": [
            {
                "name": "Account",
                "kind": "struct",
                "fields": [
                    {
                        "name": "balance",
                        "kind": "i64",
                        "nullable": false
                    }
                ]
            }
        ],
        "transactionFunctions": ["transfer"],
        "roots": [
            {
                "name": "bank",
                "type": "Account",
                "nullable": false
            }
        ]
    }"#;

    let sidecar = parse_kotlin_sidecar(&json[..]).unwrap();
    assert_eq!(sidecar.version, 1);
    assert_eq!(sidecar.module, "transaction-kotlin-bank");
    assert_eq!(
        sidecar.persistent_types[0].kind,
        KotlinPersistentKind::Struct
    );
    assert_eq!(
        sidecar.persistent_types[0].fields[0].kind,
        KotlinFieldKind::I64
    );
}

#[test]
fn rejects_duplicate_persistent_type_names() {
    let json = br#"{
        "version": 1,
        "module": "transaction-kotlin-bank",
        "persistentTypes": [
            {
                "name": "Account",
                "kind": "struct",
                "fields": []
            },
            {
                "name": "Account",
                "kind": "struct",
                "fields": []
            }
        ],
        "transactionFunctions": ["transfer"],
        "roots": []
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("duplicate persistent type name"));
}

#[test]
fn rejects_unknown_root_type() {
    let json = br#"{
        "version": 1,
        "module": "transaction-kotlin-bank",
        "persistentTypes": [],
        "transactionFunctions": ["transfer"],
        "roots": [
            {
                "name": "bank",
                "type": "Bank",
                "nullable": false
            }
        ]
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("unknown root type"));
}

#[test]
fn rejects_unknown_json_fields() {
    let json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            {
                "name": "Account",
                "kind": "struct",
                "fileds": []
            }
        ],
        "transactionFunctions": [],
        "roots": []
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("unknown field"));
}

#[test]
fn rejects_duplicate_transaction_function_names() {
    let json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [],
        "transactionFunctions": ["transfer", "transfer"],
        "roots": []
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("duplicate transaction function name"));
}

#[test]
fn rejects_duplicate_root_names() {
    let json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            { "name": "Bank", "kind": "struct", "fields": [] }
        ],
        "transactionFunctions": [],
        "roots": [
            { "name": "bank", "type": "Bank", "nullable": false },
            { "name": "bank", "type": "Bank", "nullable": false }
        ]
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("duplicate root name"));
}

#[test]
fn rejects_duplicate_field_names() {
    let json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            {
                "name": "Account",
                "kind": "struct",
                "fields": [
                    { "name": "balance", "kind": "i64", "nullable": false },
                    { "name": "balance", "kind": "i64", "nullable": false }
                ]
            }
        ],
        "transactionFunctions": [],
        "roots": []
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("duplicate field name"));
}

#[test]
fn rejects_ref_fields_without_known_targets() {
    let json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            {
                "name": "Bank",
                "kind": "struct",
                "fields": [
                    { "name": "alice", "kind": "ref", "type": "Account", "nullable": false }
                ]
            }
        ],
        "transactionFunctions": [],
        "roots": []
    }"#;

    let err = parse_kotlin_sidecar(&json[..]).unwrap_err().to_string();
    assert!(err.contains("references unknown persistent type"));
}

#[test]
fn rejects_invalid_array_and_struct_layouts() {
    let struct_json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            {
                "name": "Account",
                "kind": "struct",
                "fields": [],
                "element": { "name": "item", "kind": "i64", "nullable": false }
            }
        ],
        "transactionFunctions": [],
        "roots": []
    }"#;
    let struct_err = parse_kotlin_sidecar(&struct_json[..])
        .unwrap_err()
        .to_string();
    assert!(struct_err.contains("must not define an element"));

    let array_json = br#"{
        "version": 1,
        "module": "bad",
        "persistentTypes": [
            {
                "name": "Accounts",
                "kind": "array",
                "fields": []
            }
        ],
        "transactionFunctions": [],
        "roots": []
    }"#;
    let array_err = parse_kotlin_sidecar(&array_json[..])
        .unwrap_err()
        .to_string();
    assert!(array_err.contains("must define an element"));
}

#[test]
fn rejects_empty_names() {
    let cases: &[(&[u8], &str)] = &[
        (
            br#"{
                "version": 1,
                "module": "bad",
                "persistentTypes": [
                    { "name": "", "kind": "struct", "fields": [] }
                ],
                "transactionFunctions": [],
                "roots": []
            }"#,
            "persistent type name must not be empty",
        ),
        (
            br#"{
                "version": 1,
                "module": "bad",
                "persistentTypes": [],
                "transactionFunctions": [""],
                "roots": []
            }"#,
            "transaction function name must not be empty",
        ),
        (
            br#"{
                "version": 1,
                "module": "bad",
                "persistentTypes": [
                    {
                        "name": "Account",
                        "kind": "struct",
                        "fields": [
                            { "name": "", "kind": "i64", "nullable": false }
                        ]
                    }
                ],
                "transactionFunctions": [],
                "roots": []
            }"#,
            "field name in persistent type Account must not be empty",
        ),
        (
            br#"{
                "version": 1,
                "module": "bad",
                "persistentTypes": [
                    {
                        "name": "Accounts",
                        "kind": "array",
                        "element": { "name": "", "kind": "i64", "nullable": false }
                    }
                ],
                "transactionFunctions": [],
                "roots": []
            }"#,
            "array element name in persistent type Accounts must not be empty",
        ),
        (
            br#"{
                "version": 1,
                "module": "bad",
                "persistentTypes": [
                    { "name": "Bank", "kind": "struct", "fields": [] }
                ],
                "transactionFunctions": [],
                "roots": [
                    { "name": "", "type": "Bank", "nullable": false }
                ]
            }"#,
            "root name must not be empty",
        ),
    ];

    for (json, expected) in cases {
        let err = parse_kotlin_sidecar(*json).unwrap_err().to_string();
        assert!(
            err.contains(expected),
            "expected {expected:?} in error {err:?}"
        );
    }
}
