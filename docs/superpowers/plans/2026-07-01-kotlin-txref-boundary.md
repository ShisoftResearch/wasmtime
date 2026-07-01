# Kotlin TxRef Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add explicit Kotlin `TxRef` / `make_txn_ref` support and make `twasm-kotlin rewrite` reject ordinary WasmGC references that enter persistent state without `TxRef.get()` provenance.

**Architecture:** The Kotlin SDK exposes a reusable wrapper type for values prepared outside transactions. The Kotlin Wasm rewriter performs a compile-time provenance validation pass over function bodies and call sites, then uses the existing runtime promotion path only for values proven to come from `TxRef.get()` or already-persistent sources. The bank example becomes the reference usage with a persistent `String` note.

**Tech Stack:** Rust (`wasmtime-transaction-tools`, Wasm parser/encoder), Kotlin Multiplatform Wasm/WASI SDK examples, existing Wasmtime transaction runtime tests.

---

## File Structure

- Modify `crates/transaction-tools/tests/kotlin_rewrite.rs`: add failing unit tests for `TxRef.get()` provenance, direct ordinary reference rejection, repeated `TxRef` aliasing, and call-site propagation.
- Modify `crates/transaction-tools/src/kotlin/rewrite.rs`: add TxRef function detection, local stack provenance tracking, interprocedural parameter capability inference, validation at persistent roots/fields/array elements, and clear rewrite errors.
- Modify `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`: add `TxRef<T>`, `make_txn_ref`, and `TxRef<T>.get()`.
- Modify `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`: update the bank example to use `make_txn_ref` and persist `Bank.note`.
- Modify `examples/transaction-kotlin/bank/twasm.kotlin.json`: add `gcWasm.capture = "allModuleGcTypes"` and the `Bank.note` field.
- Modify `tests/transaction_kotlin_toolset.rs`: update bank recovery checks for the new `note` field and add a focused assertion that the note reference recovers as a persistent object.

## Task 1: Rewriter TxRef Provenance Tests

**Files:**
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add a helper for named ref fields**

Add this helper near the existing `ref_field` helper:

```rust
fn nullable_ref_field(name: &str, target: &str) -> KotlinField {
    ref_field(name, target, true)
}
```

- [ ] **Step 2: Add a failing accepted test for TxRef.get into a persistent field**

Add this test near the existing generic WasmGC rewrite tests:

```rust
#[test]
fn rewrite_accepts_txref_get_into_persistent_field() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $TxRefString (struct (field (mut (ref null $String)))))
          (type $Bank (struct (field (mut (ref null $String)))))
          (func $"twasm.TxRef.get"
                (param $txref (ref $TxRefString))
                (result (ref null $String))
            local.get $txref
            struct.get $TxRefString 0)
          (func $install
                (param $note (ref null $String))
                (result (ref null $Bank))
            local.get $note
            struct.new $Bank)
          (func (export "publish")
                (param $txref (ref $TxRefString))
                (result (ref null $Bank))
            local.get $txref
            call $"twasm.TxRef.get"
            call $install))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "txref-accepted",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [
            {
              "name": "Bank",
              "kind": "struct",
              "fields": [
                { "name": "note", "kind": "ref", "type": "String", "nullable": true }
              ]
            }
          ],
          "transactionFunctions": ["publish"],
          "roots": []
        }"#
        .as_slice(),
    )
    .unwrap();

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert_valid_module(&output);
    assert!(report.rewritten_object_ops >= 1, "{report:?}");
    assert!(printed.contains("tstruct.new"), "{printed}");
}
```

- [ ] **Step 3: Run the accepted test and verify it fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_accepts_txref_get_into_persistent_field -- --nocapture
```

Expected: FAIL because the rewriter still treats the `String` argument as ordinary or does not rewrite the persistent constructor.

- [ ] **Step 4: Add a failing rejection test for direct ordinary refs**

Add this test after `rewrite_accepts_txref_get_into_persistent_field`:

```rust
#[test]
fn rewrite_rejects_direct_ordinary_ref_into_persistent_field() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $Bank (struct (field (mut (ref null $String)))))
          (func $install
                (param $note (ref null $String))
                (result (ref null $Bank))
            local.get $note
            struct.new $Bank)
          (func (export "publish") (result (ref null $Bank))
            i32.const 7
            struct.new $String
            call $install))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "txref-reject-direct",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [
            {
              "name": "Bank",
              "kind": "struct",
              "fields": [
                { "name": "note", "kind": "ref", "type": "String", "nullable": true }
              ]
            }
          ],
          "transactionFunctions": ["publish"],
          "roots": []
        }"#
        .as_slice(),
    )
    .unwrap();

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("ordinary WasmGC reference enters persistent field Bank.note without make_txn_ref"),
        "{err}"
    );
}
```

- [ ] **Step 5: Run the direct rejection test and verify it fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_rejects_direct_ordinary_ref_into_persistent_field -- --nocapture
```

Expected: FAIL because the current rewriter accepts implicit promotion.

- [ ] **Step 6: Add a failing rejection test for unknown ordinary function results**

Add this test after the direct rejection test:

```rust
#[test]
fn rewrite_rejects_unknown_function_result_into_persistent_field() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $Bank (struct (field (mut (ref null $String)))))
          (type $make_ty (func (result (ref null $String))))
          (import "fixture" "make_string" (func $make_string (type $make_ty)))
          (func (export "publish") (result (ref null $Bank))
            call $make_string
            struct.new $Bank))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "txref-reject-unknown",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [
            {
              "name": "Bank",
              "kind": "struct",
              "fields": [
                { "name": "note", "kind": "ref", "type": "String", "nullable": true }
              ]
            }
          ],
          "transactionFunctions": ["publish"],
          "roots": []
        }"#
        .as_slice(),
    )
    .unwrap();

    let err = rewrite_kotlin_module(&input, &sidecar)
        .unwrap_err()
        .to_string();

    assert!(
        err.contains("ordinary WasmGC reference enters persistent field Bank.note without make_txn_ref"),
        "{err}"
    );
}
```

- [ ] **Step 7: Run the unknown-result rejection test and verify it fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_rejects_unknown_function_result_into_persistent_field -- --nocapture
```

Expected: FAIL because the current rewriter accepts implicit promotion.

- [ ] **Step 8: Add a failing accepted test for repeated TxRef use**

Add this test after the unknown-result rejection test:

```rust
#[test]
fn rewrite_accepts_repeated_txref_get_into_same_persistent_graph() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $TxRefString (struct (field (mut (ref null $String)))))
          (type $Bank
            (struct
              (field (mut (ref null $String)))
              (field (mut (ref null $String)))))
          (func $"twasm.TxRef.get"
                (param $txref (ref $TxRefString))
                (result (ref null $String))
            local.get $txref
            struct.get $TxRefString 0)
          (func (export "publish")
                (param $txref (ref $TxRefString))
                (result (ref null $Bank))
            local.get $txref
            call $"twasm.TxRef.get"
            local.get $txref
            call $"twasm.TxRef.get"
            struct.new $Bank))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "txref-repeated",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [
            {
              "name": "Bank",
              "kind": "struct",
              "fields": [
                { "name": "note", "kind": "ref", "type": "String", "nullable": true },
                { "name": "mirror", "kind": "ref", "type": "String", "nullable": true }
              ]
            }
          ],
          "transactionFunctions": ["publish"],
          "roots": []
        }"#
        .as_slice(),
    )
    .unwrap();

    let (output, _) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    assert_valid_module(&output);
}
```

- [ ] **Step 9: Run the repeated-use test and verify it fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_accepts_repeated_txref_get_into_same_persistent_graph -- --nocapture
```

Expected: FAIL because TxRef provenance is not implemented.

- [ ] **Step 10: Commit the failing tests**

Run:

```bash
git add crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "test: cover Kotlin TxRef provenance"
```

Expected: commit succeeds with only the test file staged.

## Task 2: Rewriter Provenance Analysis And Enforcement

**Files:**
- Modify: `crates/transaction-tools/src/kotlin/rewrite.rs`
- Test: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add provenance and capability types**

Add these types near `FunctionSignature`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefProvenance {
    NonRef,
    NullRef,
    OrdinaryRef,
    PersistentRef,
    TxnRefAllowed,
    Parameter(u32),
    UnknownRef,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefCapabilityRequirement {
    PersistentReceiver,
    PersistentValue,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct FunctionRefRequirements {
    params: BTreeMap<u32, RefCapabilityRequirement>,
}

impl RefProvenance {
    fn can_enter_persistent_value(self) -> bool {
        matches!(
            self,
            RefProvenance::NullRef | RefProvenance::PersistentRef | RefProvenance::TxnRefAllowed
        )
    }

    fn can_be_persistent_receiver(self) -> bool {
        matches!(self, RefProvenance::PersistentRef | RefProvenance::NullRef)
    }
}
```

- [ ] **Step 2: Add helper functions for reference provenance**

Add these helpers near `module_ref_type_index`:

```rust
fn val_type_is_ref(ty: ParserValType) -> bool {
    matches!(ty, ParserValType::Ref(_))
}

fn local_initial_provenance(
    local_index: u32,
    param_count: u32,
    local_types: &[ParserValType],
    function_requirements: &FunctionRefRequirements,
    gc_type_info: &GcTypeInfo,
) -> RefProvenance {
    let Some(ty) = local_types.get(local_index as usize).copied() else {
        return RefProvenance::UnknownRef;
    };
    if !val_type_is_ref(ty) {
        return RefProvenance::NonRef;
    }
    if let Some(requirement) = function_requirements.params.get(&local_index).copied() {
        return match requirement {
            RefCapabilityRequirement::PersistentReceiver => RefProvenance::PersistentRef,
            RefCapabilityRequirement::PersistentValue => RefProvenance::TxnRefAllowed,
        };
    }
    if local_index < param_count {
        return RefProvenance::Parameter(local_index);
    }
    match module_ref_type_index(ty) {
        Some(type_index)
            if gc_type_info.persistent.structs.contains(&type_index)
                || gc_type_info.persistent.arrays.contains(&type_index) =>
        {
            RefProvenance::PersistentRef
        }
        _ => RefProvenance::OrdinaryRef,
    }
}

fn ref_result_provenance(
    result_ty: ParserValType,
    type_index: Option<u32>,
    gc_type_info: &GcTypeInfo,
) -> RefProvenance {
    if !val_type_is_ref(result_ty) {
        return RefProvenance::NonRef;
    }
    match type_index {
        Some(type_index)
            if gc_type_info.persistent.structs.contains(&type_index)
                || gc_type_info.persistent.arrays.contains(&type_index) =>
        {
            RefProvenance::PersistentRef
        }
        _ => RefProvenance::OrdinaryRef,
    }
}

fn ref_boundary_label_for_struct_field(
    gc_type_info: &GcTypeInfo,
    struct_type_index: u32,
    field_index: u32,
) -> String {
    let type_name = gc_type_info
        .sidecar_type_indices
        .iter()
        .find_map(|(name, index)| (*index == struct_type_index).then_some(name.as_str()))
        .unwrap_or("persistent struct");
    let field_name = gc_type_info
        .struct_field_names
        .iter()
        .find_map(|((type_index, name), indices)| {
            (*type_index == struct_type_index && indices.contains(&field_index)).then_some(name.as_str())
        })
        .unwrap_or("field");
    format!("persistent field {type_name}.{field_name}")
}
```

When calling `local_initial_provenance`, pass the real function parameter count.
Unknown reference parameters must start as `RefProvenance::Parameter(index)` so
the analysis pass can infer call-site requirements.

- [ ] **Step 3: Add TxRef.get function detection**

Add this helper near `function_indices_by_name`:

```rust
fn txref_get_function_indices(input: &[u8]) -> Result<BTreeSet<u32>> {
    function_indices_by_name(input, |name| {
        name == "twasm.TxRef.get"
            || name.ends_with(".TxRef.get")
            || name.ends_with("TxRef.<get-value>")
            || name.contains("TxRef") && name.ends_with(".get")
    })
}
```

- [ ] **Step 4: Add a validation helper for persistent value boundaries**

Add this helper near the TxRef detection helper:

```rust
fn ensure_ref_can_enter_persistent_value(
    provenance: RefProvenance,
    boundary: impl std::fmt::Display,
) -> Result<()> {
    ensure!(
        provenance.can_enter_persistent_value(),
        "ordinary WasmGC reference enters {boundary} without make_txn_ref"
    );
    Ok(())
}

fn ensure_ref_can_be_persistent_receiver(
    provenance: RefProvenance,
    boundary: impl std::fmt::Display,
) -> Result<()> {
    ensure!(
        provenance.can_be_persistent_receiver(),
        "ordinary WasmGC reference used as {boundary} without persistent provenance"
    );
    Ok(())
}
```

Add recording variants for the analysis pass:

```rust
fn require_or_record_persistent_value(
    provenance: RefProvenance,
    requirements: &mut FunctionRefRequirements,
    boundary: impl std::fmt::Display,
) -> Result<()> {
    match provenance {
        RefProvenance::Parameter(index) => {
            requirements
                .params
                .insert(index, RefCapabilityRequirement::PersistentValue);
            Ok(())
        }
        provenance => ensure_ref_can_enter_persistent_value(provenance, boundary),
    }
}

fn require_or_record_persistent_receiver(
    provenance: RefProvenance,
    requirements: &mut FunctionRefRequirements,
    boundary: impl std::fmt::Display,
) -> Result<()> {
    match provenance {
        RefProvenance::Parameter(index) => {
            requirements
                .params
                .insert(index, RefCapabilityRequirement::PersistentReceiver);
            Ok(())
        }
        provenance => ensure_ref_can_be_persistent_receiver(provenance, boundary),
    }
}
```

- [ ] **Step 5: Add a function-body analysis helper**

Add a helper that scans one function body and returns both parameter requirements and validation errors. Place it after `rewrite_function_body` so it can reuse the local type and operator helpers:

```rust
fn analyze_function_ref_requirements(
    body: &wasmparser::FunctionBody<'_>,
    params: &[ParserValType],
    gc_type_info: &GcTypeInfo,
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    known_requirements: &BTreeMap<u32, FunctionRefRequirements>,
    txref_get_functions: &BTreeSet<u32>,
) -> Result<FunctionRefRequirements> {
    let local_types = local_types(body, params)?;
    let mut local_provenance = (0..local_types.len())
        .map(|index| {
            let index = u32::try_from(index).unwrap();
            local_initial_provenance(
                index,
                u32::try_from(params.len()).unwrap(),
                &local_types,
                &FunctionRefRequirements::default(),
                gc_type_info,
            )
        })
        .collect::<Vec<_>>();
    let mut stack = Vec::<RefProvenance>::new();
    let mut requirements = FunctionRefRequirements::default();
    let mut reader = body.get_operators_reader()?;
    while !reader.eof() {
        let op = reader.read()?;
        update_provenance_for_operator(
            &op,
            &local_types,
            &mut local_provenance,
            &mut stack,
            &mut requirements,
            gc_type_info,
            function_signatures,
            known_requirements,
            txref_get_functions,
        )?;
    }
    Ok(requirements)
}
```

- [ ] **Step 6: Add the stack update function used by analysis**

Add this helper after `analyze_function_ref_requirements`:

```rust
fn update_provenance_for_operator(
    op: &Operator<'_>,
    local_types: &[ParserValType],
    local_provenance: &mut [RefProvenance],
    stack: &mut Vec<RefProvenance>,
    requirements: &mut FunctionRefRequirements,
    gc_type_info: &GcTypeInfo,
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    known_requirements: &BTreeMap<u32, FunctionRefRequirements>,
    txref_get_functions: &BTreeSet<u32>,
) -> Result<()> {
    match op {
        Operator::LocalGet { local_index } => {
            stack.push(
                local_provenance
                    .get(*local_index as usize)
                    .copied()
                    .unwrap_or(RefProvenance::UnknownRef),
            );
        }
        Operator::LocalSet { local_index } => {
            let value = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            if let Some(slot) = local_provenance.get_mut(*local_index as usize) {
                *slot = value;
            }
        }
        Operator::LocalTee { local_index } => {
            let value = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            if let Some(slot) = local_provenance.get_mut(*local_index as usize) {
                *slot = value;
            }
            stack.push(value);
        }
        Operator::Drop => {
            stack.pop();
        }
        Operator::RefNull { .. } => stack.push(RefProvenance::NullRef),
        Operator::StructNew { struct_type_index } => {
            validate_persistent_struct_constructor_values(
                *struct_type_index,
                stack,
                gc_type_info,
                requirements,
            )?;
            let constructed = if gc_type_info.persistent.structs.contains(struct_type_index) {
                RefProvenance::PersistentRef
            } else {
                RefProvenance::OrdinaryRef
            };
            stack.push(constructed);
        }
        Operator::StructSet {
            struct_type_index,
            field_index,
        } if gc_type_info.persistent.structs.contains(struct_type_index) => {
            let value = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            let receiver = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            if gc_type_info
                .struct_fields
                .get(&(*struct_type_index, *field_index))
                .copied()
                .is_some_and(val_type_is_ref)
            {
                let label =
                    ref_boundary_label_for_struct_field(gc_type_info, *struct_type_index, *field_index);
                require_or_record_persistent_value(value, requirements, label)?;
            }
            require_or_record_persistent_receiver(receiver, requirements, "persistent struct receiver")?;
        }
        Operator::ArraySet { array_type_index }
            if gc_type_info.persistent.arrays.contains(array_type_index) =>
        {
            let value = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            let _index = stack.pop();
            let receiver = stack.pop().unwrap_or(RefProvenance::UnknownRef);
            if gc_type_info
                .array_elements
                .get(array_type_index)
                .copied()
                .is_some_and(val_type_is_ref)
            {
                require_or_record_persistent_value(value, requirements, "persistent array element")?;
            }
            require_or_record_persistent_receiver(receiver, requirements, "persistent array receiver")?;
        }
        Operator::Call { function_index } => {
            apply_call_provenance(
                *function_index,
                stack,
                gc_type_info,
                function_signatures,
                known_requirements,
                txref_get_functions,
                ProvenanceCallMode::RecordCallerRequirements(requirements),
            )?;
        }
        Operator::I32Const { .. }
        | Operator::I64Const { .. }
        | Operator::F32Const { .. }
        | Operator::F64Const { .. }
        | Operator::V128Const { .. } => stack.push(RefProvenance::NonRef),
        _ => {
            stack.clear();
            for provenance in local_provenance {
                if matches!(*provenance, RefProvenance::OrdinaryRef | RefProvenance::TxnRefAllowed) {
                    *provenance = RefProvenance::UnknownRef;
                }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 7: Add call-site provenance handling**

Add this helper after `update_provenance_for_operator`:

```rust
fn apply_call_provenance(
    function_index: u32,
    stack: &mut Vec<RefProvenance>,
    gc_type_info: &GcTypeInfo,
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    known_requirements: &BTreeMap<u32, FunctionRefRequirements>,
    txref_get_functions: &BTreeSet<u32>,
    mode: ProvenanceCallMode,
) -> Result<()> {
    let Some(signature) = function_signatures.get(&function_index) else {
        stack.clear();
        return Ok(());
    };
    let mut args = Vec::with_capacity(signature.params.len());
    for _ in &signature.params {
        args.push(stack.pop().unwrap_or(RefProvenance::UnknownRef));
    }
    args.reverse();
    if let Some(requirements) = known_requirements.get(&function_index) {
        for (param_index, requirement) in &requirements.params {
            let provenance = args
                .get(*param_index as usize)
                .copied()
                .unwrap_or(RefProvenance::UnknownRef);
            match requirement {
                RefCapabilityRequirement::PersistentReceiver => {
                    match mode {
                        ProvenanceCallMode::RecordCallerRequirements(caller_requirements) => {
                            require_or_record_persistent_receiver(
                                provenance,
                                caller_requirements,
                                "persistent parameter",
                            )?
                        }
                        ProvenanceCallMode::ValidateOnly => {
                            ensure_ref_can_be_persistent_receiver(provenance, "persistent parameter")?
                        }
                    }
                }
                RefCapabilityRequirement::PersistentValue => {
                    match mode {
                        ProvenanceCallMode::RecordCallerRequirements(caller_requirements) => {
                            require_or_record_persistent_value(
                                provenance,
                                caller_requirements,
                                "persistent parameter",
                            )?
                        }
                        ProvenanceCallMode::ValidateOnly => {
                            ensure_ref_can_enter_persistent_value(provenance, "persistent parameter")?
                        }
                    }
                }
            }
        }
    }
    if txref_get_functions.contains(&function_index) {
        for result in &signature.results {
            if val_type_is_ref(*result) {
                stack.push(RefProvenance::TxnRefAllowed);
            } else {
                stack.push(RefProvenance::NonRef);
            }
        }
        return Ok(());
    }
    for result in &signature.results {
        let type_index = module_ref_type_index(*result);
        stack.push(ref_result_provenance(*result, type_index, gc_type_info));
    }
    Ok(())
}
```

- [ ] **Step 8: Add persistent constructor value validation**

Add this helper after `apply_call_provenance`:

```rust
fn validate_persistent_struct_constructor_values(
    struct_type_index: u32,
    stack: &mut Vec<RefProvenance>,
    gc_type_info: &GcTypeInfo,
    requirements: &mut FunctionRefRequirements,
) -> Result<()> {
    let field_count = gc_type_info
        .struct_field_counts
        .get(&struct_type_index)
        .copied()
        .unwrap_or(0);
    let mut values = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        values.push(stack.pop().unwrap_or(RefProvenance::UnknownRef));
    }
    values.reverse();
    if !gc_type_info.persistent.structs.contains(&struct_type_index) {
        return Ok(());
    }
    for (field_index, value) in values.into_iter().enumerate() {
        let field_index = u32::try_from(field_index).context("persistent field index overflow")?;
        let Some(field_ty) = gc_type_info
            .struct_fields
            .get(&(struct_type_index, field_index))
            .copied()
        else {
            continue;
        };
        if !val_type_is_ref(field_ty) {
            continue;
        }
        let label = ref_boundary_label_for_struct_field(gc_type_info, struct_type_index, field_index);
        require_or_record_persistent_value(value, requirements, label)?;
    }
    Ok(())
}
```

Add this mode enum before `apply_call_provenance`:

```rust
enum ProvenanceCallMode<'a> {
    RecordCallerRequirements(&'a mut FunctionRefRequirements),
    ValidateOnly,
}
```

Use `ProvenanceCallMode::RecordCallerRequirements(requirements)` from the
analysis pass and `ProvenanceCallMode::ValidateOnly` from the rewrite pass. In
the analysis pass, if a callee requires a persistent value and the caller
argument is `RefProvenance::Parameter(index)`, record the same requirement on
the caller parameter instead of rejecting it. In the rewrite validation pass,
the current function's inferred parameter requirements have already converted
required parameters to `PersistentRef` or `TxnRefAllowed`, so a remaining
`RefProvenance::Parameter(index)` at a callee boundary must become a rewrite
error.

- [ ] **Step 9: Build function-index signatures**

Add this helper near `function_type_signatures`:

```rust
fn function_index_signatures(input: &[u8]) -> Result<BTreeMap<u32, FunctionSignature>> {
    let type_signatures = function_type_signatures(input)?;
    let imported_types = imported_function_type_indices(input)?;
    let defined_types = defined_function_type_indices(input)?;
    let mut signatures = BTreeMap::new();
    for (index, type_index) in imported_types
        .into_iter()
        .chain(defined_types.into_iter())
        .enumerate()
    {
        let index = u32::try_from(index).context("Kotlin rewrite function index overflow")?;
        if let Some(signature) = type_signatures.get(&type_index).cloned() {
            signatures.insert(index, signature);
        }
    }
    Ok(signatures)
}
```

- [ ] **Step 10: Add a module-wide fixed-point requirements pass**

Add this helper near `transaction_call_closure_function_indices`:

```rust
fn function_ref_requirements(
    input: &[u8],
    imported_function_count: u32,
    func_type_params: &BTreeMap<u32, Vec<ParserValType>>,
    defined_function_types: &[u32],
    gc_type_info: &GcTypeInfo,
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    txref_get_functions: &BTreeSet<u32>,
) -> Result<BTreeMap<u32, FunctionRefRequirements>> {
    let mut requirements = BTreeMap::<u32, FunctionRefRequirements>::new();
    for _ in 0..8 {
        let previous = requirements.clone();
        let mut next_defined_func = 0u32;
        for payload in Parser::new(0).parse_all(input) {
            if let Payload::CodeSectionStart { range, .. } =
                payload.context("failed to parse Kotlin provenance payload")?
            {
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid Kotlin provenance code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;
                for body in section {
                    let body = body?;
                    let function_index = imported_function_count
                        .checked_add(next_defined_func)
                        .context("Kotlin provenance function index overflow")?;
                    let type_index = *defined_function_types
                        .get(next_defined_func as usize)
                        .context("missing Kotlin provenance function type")?;
                    let params = func_type_params
                        .get(&type_index)
                        .cloned()
                        .context("missing Kotlin provenance function parameters")?;
                    let analyzed = analyze_function_ref_requirements(
                        &body,
                        &params,
                        gc_type_info,
                        function_signatures,
                        &previous,
                        txref_get_functions,
                    )?;
                    if !analyzed.params.is_empty() {
                        requirements.insert(function_index, analyzed);
                    }
                    next_defined_func += 1;
                }
            }
        }
        if requirements == previous {
            return Ok(requirements);
        }
    }
    Ok(requirements)
}
```

- [ ] **Step 11: Thread provenance data into `rewrite_kotlin_module`**

In `rewrite_kotlin_module`, after `func_type_params` and `defined_function_types` are initialized, add:

```rust
let function_signatures = function_index_signatures(input)?;
let txref_get_functions = txref_get_function_indices(input)?;
let ref_requirements = function_ref_requirements(
    input,
    imported_function_count,
    &func_type_params,
    &defined_function_types,
    &gc_type_info,
    &function_signatures,
    &txref_get_functions,
)?;
```

Then pass `&function_signatures`, `&ref_requirements`, and `&txref_get_functions` into `rewrite_function_body`.

- [ ] **Step 12: Extend `rewrite_function_body` signature and validation**

Change `rewrite_function_body` to accept:

```rust
function_signatures: &BTreeMap<u32, FunctionSignature>,
ref_requirements: &BTreeMap<u32, FunctionRefRequirements>,
txref_get_functions: &BTreeSet<u32>,
```

Inside `rewrite_function_body`, initialize local provenance:

```rust
let function_requirements = ref_requirements
    .get(&function_index)
    .cloned()
    .unwrap_or_default();
let mut value_stack = Vec::<RefProvenance>::new();
let mut local_provenance = (0..local_types.len())
    .map(|index| {
        local_initial_provenance(
            u32::try_from(index).unwrap(),
            u32::try_from(params.len()).unwrap(),
            &local_types,
            &function_requirements,
            gc_type_info,
        )
    })
    .collect::<Vec<_>>();
```

If adding `function_index` to the existing function signature is cleaner, pass it from the call site alongside `params`.

Before emitting persistent `StructSet`, `ArraySet`, and persistent `StructNew`, validate the relevant reference operands using the helpers above. For operators not specially handled by the rewriter, call `update_provenance_for_operator` before or after emitting the instruction in the same order as the Wasm stack effect.

- [ ] **Step 13: Run TxRef tests and fix compile errors**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite txref -- --nocapture
```

Expected: all TxRef tests PASS.

- [ ] **Step 14: Run the full Kotlin rewriter test file**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --nocapture
```

Expected: all tests PASS.

- [ ] **Step 15: Commit the rewriter implementation**

Run:

```bash
git add crates/transaction-tools/src/kotlin/rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "feat: enforce Kotlin TxRef provenance"
```

Expected: commit succeeds with only rewriter source and rewriter tests staged.

## Task 3: Kotlin SDK And Bank Example

**Files:**
- Modify: `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`
- Modify: `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`
- Modify: `examples/transaction-kotlin/bank/twasm.kotlin.json`

- [ ] **Step 1: Add the Kotlin SDK API**

Modify `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt` to include:

```kotlin
class TxRef<T> internal constructor(val value: T)

fun <T> make_txn_ref(value: T): TxRef<T> =
    TxRef(value)

fun <T> TxRef<T>.get(): T =
    value
```

Place this after the `TxnFunc` annotation and before `transaction`.

- [ ] **Step 2: Update the bank example source**

Replace `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt` with this content:

```kotlin
import twasm.Persistent
import twasm.TxnFunc
import twasm.make_txn_ref
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
    var note: String
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long, note: String) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
    bank.note = note
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long, note: String) {
    val bank = Bank(Account(alice), Account(bob), note)
    setRoot("bank", bank)
}

fun main() {
    val note = make_txn_ref("this is a note")
    transaction {
        installFreshBank(1_000, 200, note.get())
        val bank = root<Bank>("bank")
        transfer(bank, true, 100, note.get())
    }
}
```

- [ ] **Step 3: Update the bank sidecar**

Replace `examples/transaction-kotlin/bank/twasm.kotlin.json` with:

```json
{
  "version": 1,
  "module": "transaction-kotlin-bank",
  "gcWasm": {
    "capture": "allModuleGcTypes",
    "denyTypes": [
      "kotlin.coroutines.*",
      "kotlin.Throwable"
    ]
  },
  "persistentTypes": [
    {
      "name": "Account",
      "kind": "struct",
      "fields": [
        { "name": "balance", "kind": "i64", "nullable": false }
      ]
    },
    {
      "name": "Bank",
      "kind": "struct",
      "fields": [
        { "name": "alice", "kind": "ref", "type": "Account", "nullable": true },
        { "name": "bob", "kind": "ref", "type": "Account", "nullable": true },
        { "name": "note", "kind": "ref", "type": "kotlin.String", "nullable": true }
      ]
    }
  ],
  "transactionFunctions": [
    "transfer",
    "installFreshBank"
  ],
  "roots": [
    { "name": "bank", "type": "Bank", "nullable": false }
  ]
}
```

- [ ] **Step 4: Build the Kotlin bank example**

Run:

```bash
cd examples/transaction-kotlin
./gradlew :bank:compileDevelopmentExecutableKotlinWasmWasi
```

Expected: build succeeds.

- [ ] **Step 5: Inspect generated names if TxRef tests fail against real Kotlin**

Run:

```bash
cargo run -p wasmtime-transaction-tools --bin twasm-kotlin -- inspect examples/transaction-kotlin/bank/twasm.kotlin.json --wasm examples/transaction-kotlin/bank/build/compileSync/wasmWasi/main/developmentExecutable/kotlin/transaction-kotlin-bank.wasm
```

Expected: inspect succeeds. If the generated Wasm path differs, locate the single `.wasm` artifact under `examples/transaction-kotlin/bank/build` with:

```bash
find examples/transaction-kotlin/bank/build -name '*.wasm' -print
```

- [ ] **Step 6: Commit SDK and example updates**

Run:

```bash
git add examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt \
  examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt \
  examples/transaction-kotlin/bank/twasm.kotlin.json
git commit -m "feat: add Kotlin TxRef SDK usage"
```

Expected: commit succeeds with only SDK and bank example files staged.

## Task 4: Kotlin Bank Runtime Recovery Assertions

**Files:**
- Modify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Update recovered bank field count**

In `kotlin_rewritten_bank_executes_and_recovers_file_backed_roots`, change the recovered field reads from:

```rust
let alice_ref = recovered_object_ref_field(&root_winner.record_bytes, 2, 0)?
    .context("recovered Bank alice field was null")?;
let bob_ref = recovered_object_ref_field(&root_winner.record_bytes, 2, 1)?
    .context("recovered Bank bob field was null")?;
```

to:

```rust
let alice_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 0)?
    .context("recovered Bank alice field was null")?;
let bob_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 1)?
    .context("recovered Bank bob field was null")?;
let note_ref = recovered_object_ref_field(&root_winner.record_bytes, 3, 2)?
    .context("recovered Bank note field was null")?;
```

- [ ] **Step 2: Assert that the note object was recovered**

After the `alice_ref != bob_ref` assertion, add:

```rust
ensure!(
    recovered
        .object_winners
        .iter()
        .any(|winner| winner.object_id == note_ref),
    "recovered Bank note ObjectId did not have a persistent record"
);
```

- [ ] **Step 3: Update the recovered record count message**

Change:

```rust
"expected recovered Bank and Account records, got {}",
```

to:

```rust
"expected recovered Bank, Account, and note records, got {}",
```

- [ ] **Step 4: Run the focused bank runtime test and verify it passes**

Run:

```bash
cargo test --test transaction_kotlin_toolset kotlin_rewritten_bank_executes_and_recovers_file_backed_roots --features transaction -- --nocapture
```

Expected: PASS, or SKIP with the existing Gradle-unavailable message if Gradle is unavailable.

- [ ] **Step 5: Run Kotlin toolset tests**

Run:

```bash
cargo test --test transaction_kotlin_toolset --features transaction -- --nocapture
```

Expected: PASS, with existing Gradle skips allowed only when the test prints the established skip message.

- [ ] **Step 6: Commit runtime test updates**

Run:

```bash
git add tests/transaction_kotlin_toolset.rs
git commit -m "test: verify Kotlin TxRef bank recovery"
```

Expected: commit succeeds with only `tests/transaction_kotlin_toolset.rs` staged.

## Task 5: Final Verification And Cleanup

**Files:**
- Verify: `crates/transaction-tools/src/kotlin/rewrite.rs`
- Verify: `crates/transaction-tools/tests/kotlin_rewrite.rs`
- Verify: `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`
- Verify: `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`
- Verify: `examples/transaction-kotlin/bank/twasm.kotlin.json`
- Verify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --all
```

Expected: exits successfully.

- [ ] **Step 2: Run transaction-tools tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: Run Kotlin toolset tests**

Run:

```bash
cargo test --test transaction_kotlin_toolset --features transaction -- --nocapture
```

Expected: PASS, with existing Gradle skips allowed only when Gradle is unavailable.

- [ ] **Step 4: Run a targeted full-package check**

Run:

```bash
cargo check -p wasmtime-transaction-tools
```

Expected: PASS.

- [ ] **Step 5: Review staged and unstaged changes**

Run:

```bash
git status --short
```

Expected: only intended TxRef implementation files are modified in the implementation worktree.

- [ ] **Step 6: Commit formatting changes if any**

If `cargo fmt --all` changed files, run:

```bash
git add crates/transaction-tools/src/kotlin/rewrite.rs \
  crates/transaction-tools/tests/kotlin_rewrite.rs \
  tests/transaction_kotlin_toolset.rs
git commit -m "style: format Kotlin TxRef changes"
```

Expected: commit succeeds only when formatting produced changes.

- [ ] **Step 7: Record verification output**

Capture the final commands and outcomes in the task handoff:

```text
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --nocapture
cargo test --test transaction_kotlin_toolset --features transaction -- --nocapture
cargo check -p wasmtime-transaction-tools
```

Expected: the final response reports pass, skip, or failure for each command exactly.
