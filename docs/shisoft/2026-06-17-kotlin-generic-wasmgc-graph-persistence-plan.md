# Kotlin Generic WasmGC Graph Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Kotlin `String` and standard collection object graphs persist through the existing WasmGC object promotion/recovery path without per-container storage encodings.

**Architecture:** Add a Kotlin sidecar `gcWasm` policy that can opt a module into generic WasmGC graph persistence. In this mode the Kotlin lowerer treats all concrete module `struct`/`array` layouts as persistent-capable except denied names, rewrites WasmGC object operations across the transaction call closure, and relies on the existing Wasmtime ordinary-GC promotion adapter to snapshot reachable `struct`/`array` graphs into `ObjectId` records.

**Tech Stack:** Rust `wasmparser`/`wasm-encoder`, `wasmtime-transaction-tools`, Kotlin/WasmGC Gradle example, Wasmtime `transaction` feature, existing file-backed object recovery tests.

---

## Scope

Implement generic capture of Kotlin/WasmGC object graphs under explicit sidecar opt-in:

```json
{
  "gcWasm": {
    "capture": "allModuleGcTypes",
    "denyTypes": [
      "kotlin.coroutines.*",
      "kotlin.Throwable"
    ]
  }
}
```

This is intentionally aggressive. Under this policy Kotlin `String`, `ArrayList`,
`HashMap`, backing arrays, nodes, and user objects are just WasmGC structs/arrays.
The runtime persists their field/element graph and recovery requires the same
module/type-layout fingerprint.

Do not implement container-specific durable formats in this plan.

## Files

- Modify: `crates/transaction-tools/src/kotlin/metadata.rs`
  - Add `gc_wasm` sidecar policy.
- Modify: `crates/transaction-tools/tests/kotlin_metadata.rs`
  - Validate the new policy and old-sidecar compatibility.
- Modify: `crates/transaction-tools/src/kotlin/rewrite.rs`
  - Discover all module GC layouts.
  - Allow sidecar roots/fields to reference discovered module GC type names.
  - Rewrite transaction call-closure functions.
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`
  - Add hand-WAT tests for generic refs and closure rewriting.
- Create: `examples/transaction-kotlin/collections/...`
  - Kotlin example using `String`, `MutableList<String>`, and `MutableMap<String, String>`.
- Create: `tests/transaction_kotlin_collections.rs`
  - End-to-end build, rewrite, execute, recover tests.
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-design.md`
  - Replace "collections and strings out of scope" with the new generic graph mode.
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`
  - Record the new mode after verification.

---

## Task 1: Add Sidecar Policy For Generic WasmGC Capture

**Files:**
- Modify: `crates/transaction-tools/src/kotlin/metadata.rs`
- Modify: `crates/transaction-tools/tests/kotlin_metadata.rs`

- [ ] **Step 1: Add failing metadata tests**

Add tests:

```rust
#[test]
fn parses_generic_wasmgc_capture_policy() {
    let json = br#"{
        "version": 1,
        "module": "kotlin-collections",
        "gcWasm": {
            "capture": "allModuleGcTypes",
            "denyTypes": ["kotlin.coroutines.*", "kotlin.Throwable"]
        },
        "persistentTypes": [],
        "transactionFunctions": ["main"],
        "roots": [
            { "name": "profile", "type": "Profile", "nullable": true }
        ]
    }"#;

    let sidecar = parse_kotlin_sidecar(&json[..]).unwrap();
    assert_eq!(sidecar.gc_wasm.capture, KotlinGcWasmCapture::AllModuleGcTypes);
    assert_eq!(
        sidecar.gc_wasm.deny_types,
        vec!["kotlin.coroutines.*", "kotlin.Throwable"]
    );
}

#[test]
fn old_sidecars_default_to_sidecar_only_capture() {
    let json = br#"{
        "version": 1,
        "module": "old",
        "persistentTypes": [
            { "name": "Root", "kind": "struct", "fields": [] }
        ],
        "transactionFunctions": [],
        "roots": [
            { "name": "root", "type": "Root", "nullable": true }
        ]
    }"#;

    let sidecar = parse_kotlin_sidecar(&json[..]).unwrap();
    assert_eq!(sidecar.gc_wasm.capture, KotlinGcWasmCapture::SidecarTypes);
    assert!(sidecar.gc_wasm.deny_types.is_empty());
}
```

- [ ] **Step 2: Verify the new tests fail**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_metadata gc_wasm -- --format terse
```

Expected: compile failure for missing `KotlinGcWasmCapture` or missing `gc_wasm`.

- [ ] **Step 3: Implement metadata policy**

In `crates/transaction-tools/src/kotlin/metadata.rs`, add:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinGcWasmPolicy {
    #[serde(default)]
    pub capture: KotlinGcWasmCapture,
    #[serde(default)]
    pub deny_types: Vec<String>,
}

impl Default for KotlinGcWasmPolicy {
    fn default() -> Self {
        Self {
            capture: KotlinGcWasmCapture::SidecarTypes,
            deny_types: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinGcWasmCapture {
    SidecarTypes,
    AllModuleGcTypes,
}

impl Default for KotlinGcWasmCapture {
    fn default() -> Self {
        Self::SidecarTypes
    }
}
```

Add to `KotlinSidecar`:

```rust
#[serde(default)]
pub gc_wasm: KotlinGcWasmPolicy,
```

Update validation so unknown root/ref target names are allowed only when
`sidecar.gc_wasm.capture == KotlinGcWasmCapture::AllModuleGcTypes`; concrete
Wasm type resolution happens in the rewrite pass.

- [ ] **Step 4: Verify metadata tests pass**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_metadata -- --format terse
```

Expected: all metadata tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools/src/kotlin/metadata.rs crates/transaction-tools/tests/kotlin_metadata.rs
git commit -m "Add Kotlin generic WasmGC capture policy"
```

---

## Task 2: Discover And Mark All Module GC Layouts In Generic Mode

**Files:**
- Modify: `crates/transaction-tools/src/kotlin/rewrite.rs`
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add failing rewrite test for runtime type refs**

Add a hand-WAT test where a sidecar persistent root references a GC type not
listed in `persistentTypes`:

```rust
#[test]
fn generic_wasmgc_mode_allows_roots_and_refs_to_runtime_gc_types() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $Profile (struct (field (mut (ref null $String)))))
          (global $source (ref $Profile)
            (struct.new $Profile
              (struct.new $String (i32.const 5))))
          (func (export "publish")
            global.get $source
            drop))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "generic",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [],
          "transactionFunctions": ["publish"],
          "roots": [
            { "name": "profile", "type": "Profile", "nullable": true }
          ]
        }"#
        .as_slice(),
    )
    .unwrap();

    let (_output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    assert_eq!(report.persistent_types, 2);
}
```

- [ ] **Step 2: Verify the test fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite generic_wasmgc_mode_allows -- --exact --format terse
```

Expected: failure resolving `Profile` because it is not in `persistentTypes`.

- [ ] **Step 3: Implement all-module GC discovery**

In `GcTypeInfo`, add:

```rust
module_type_indices: BTreeMap<String, u32>,
denied_type_indices: BTreeSet<u32>,
```

In `gc_type_info`, after name-section type discovery:

- collect all struct/array type indices,
- map names from `name_section_type_names`,
- if `gc_wasm.capture == AllModuleGcTypes`, insert every non-denied struct/array
  index into `info.persistent.structs` or `info.persistent.arrays`,
- set `info.sidecar_type_indices` for both sidecar-declared names and runtime
  names used by roots/fields.

Deny matching should support exact names and a trailing `.*` prefix pattern:

```rust
fn kotlin_type_denied(name: &str, deny_types: &[String]) -> bool {
    deny_types.iter().any(|pattern| {
        pattern
            .strip_suffix(".*")
            .is_some_and(|prefix| name == prefix || name.starts_with(&format!("{prefix}.")))
            || pattern == name
    })
}
```

- [ ] **Step 4: Verify rewrite tests pass**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite generic_wasmgc_mode_allows -- --exact --format terse
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools/src/kotlin/rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "Discover generic Kotlin WasmGC layouts"
```

---

## Task 3: Rewrite Transaction Call Closure

**Files:**
- Modify: `crates/transaction-tools/src/kotlin/rewrite.rs`
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add failing closure-rewrite test**

Add a test where the exported transaction function calls a helper that mutates
an array. The helper is not listed in the sidecar but must be rewritten because
it is in the transaction call closure:

```rust
#[test]
fn generic_wasmgc_rewrites_helpers_reachable_from_transaction_functions() {
    let input = wat::parse_str(
        r#"
        (module
          (type $String (struct (field (mut i32))))
          (type $List (array (mut (ref null $String))))
          (func $push (param $list (ref $List)) (param $value (ref null $String))
            local.get $list
            i32.const 0
            local.get $value
            array.set $List)
          (func (export "publish") (param $list (ref $List))
            local.get $list
            i32.const 7
            struct.new $String
            call $push))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
        br#"{
          "version": 1,
          "module": "generic-closure",
          "gcWasm": { "capture": "allModuleGcTypes", "denyTypes": [] },
          "persistentTypes": [],
          "transactionFunctions": ["publish"],
          "roots": []
        }"#
        .as_slice(),
    )
    .unwrap();

    let (output, _) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();
    assert!(printed.contains("tarray.set"), "{printed}");
    assert!(transaction_objects(&output).functions.len() >= 2);
}
```

- [ ] **Step 2: Verify the test fails**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite generic_wasmgc_rewrites_helpers -- --exact --format terse
```

Expected: output still contains ordinary `array.set` in `$push`.

- [ ] **Step 3: Implement call graph closure**

In `rewrite_kotlin_module`:

- compute `transaction_function_indices` as today,
- parse code bodies and collect `Operator::Call { function_index }` edges for
  every defined function,
- compute a worklist closure from transaction functions to local defined
  callees,
- add closure functions to `object_rewrite_function_indices`,
- add closure functions to `transaction_objects.functions` after remapping.

Do not include imported functions in the closure. For `call_indirect`,
`return_call_indirect`, or table dispatch in a closure function, leave the
callee unknown and require a later explicit annotation if it mutates persistent
objects.

- [ ] **Step 4: Verify closure rewrite passes**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite generic_wasmgc_rewrites_helpers -- --exact --format terse
```

Expected: pass.

- [ ] **Step 5: Run full Kotlin rewrite suite**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --format terse
```

Expected: all rewrite tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/transaction-tools/src/kotlin/rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "Rewrite Kotlin transaction call closures"
```

---

## Task 4: Add Real Kotlin Collections Example

**Files:**
- Modify: `examples/transaction-kotlin/settings.gradle.kts`
- Create: `examples/transaction-kotlin/collections/build.gradle.kts`
- Create: `examples/transaction-kotlin/collections/src/wasmWasiMain/kotlin/Main.kt`
- Create: `examples/transaction-kotlin/collections/twasm.kotlin.json`

- [ ] **Step 1: Add Gradle module**

Append to `examples/transaction-kotlin/settings.gradle.kts`:

```kotlin
include(":collections")
```

Create `examples/transaction-kotlin/collections/build.gradle.kts`:

```kotlin
@file:OptIn(org.jetbrains.kotlin.gradle.ExperimentalWasmDsl::class)

plugins {
    kotlin("multiplatform")
}

kotlin {
    wasmWasi {
        nodejs()
        binaries.executable()
    }

    sourceSets {
        wasmWasiMain.dependencies {
            implementation(project(":sdk"))
        }
    }
}
```

- [ ] **Step 2: Add Kotlin source**

Create `examples/transaction-kotlin/collections/src/wasmWasiMain/kotlin/Main.kt`:

```kotlin
import twasm.Persistent
import twasm.TxnFunc
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Profile(
    var name: String,
    var tags: MutableList<String>,
    var attrs: MutableMap<String, String>,
)

@TxnFunc
fun installProfile() {
    val tags = mutableListOf("wasm", "pmem")
    val attrs = mutableMapOf("tier" to "research")
    setRoot("profile", Profile("alice", tags, attrs))
}

@TxnFunc
fun mutateProfile() {
    val profile = root<Profile>("profile")
    profile.tags.add("gc")
    profile.attrs["status"] = "active"
    profile.name = "alice-updated"
}

fun main() {
    transaction {
        installProfile()
        mutateProfile()
    }
}
```

- [ ] **Step 3: Add sidecar**

Create `examples/transaction-kotlin/collections/twasm.kotlin.json`:

```json
{
  "version": 1,
  "module": "transaction-kotlin-collections",
  "gcWasm": {
    "capture": "allModuleGcTypes",
    "denyTypes": [
      "kotlin.coroutines.*",
      "kotlin.Throwable"
    ]
  },
  "persistentTypes": [
    {
      "name": "Profile",
      "kind": "struct",
      "fields": [
        { "name": "name", "kind": "ref", "type": "kotlin.String", "nullable": true },
        { "name": "tags", "kind": "ref", "type": "kotlin.collections.ArrayList", "nullable": true },
        { "name": "attrs", "kind": "ref", "type": "kotlin.collections.LinkedHashMap", "nullable": true }
      ]
    }
  ],
  "transactionFunctions": [
    "installProfile",
    "mutateProfile"
  ],
  "roots": [
    { "name": "profile", "type": "Profile", "nullable": true }
  ]
}
```

If the actual Kotlin/Wasm type names differ, update only this sidecar after
inspecting the generated name section; do not add container-specific runtime
code.

- [ ] **Step 4: Verify the Kotlin module builds**

Run:

```bash
gradle :collections:compileDevelopmentExecutableKotlinWasmWasi -p examples/transaction-kotlin
```

Expected: Gradle build success.

- [ ] **Step 5: Commit**

```bash
git add examples/transaction-kotlin
git commit -m "Add Kotlin collections persistence example"
```

---

## Task 5: End-To-End Recovery Test For Strings And Collections

**Files:**
- Create: `tests/transaction_kotlin_collections.rs`
- Modify only if needed: `tests/transaction_kotlin_toolset.rs` helper extraction

- [ ] **Step 1: Add failing integration test**

Create `tests/transaction_kotlin_collections.rs` with a test that:

- builds `:collections`,
- rewrites the Wasm with `twasm-kotlin`,
- instantiates under Wasmtime with `wasm_gc`, reference types, function refs,
  and transactions,
- executes `_start`,
- reopens the file-backed transaction log,
- verifies at least one root and multiple object records were recovered.

Use the same storage setup helpers already used in
`tests/transaction_kotlin_toolset.rs`:

```rust
#[test]
fn kotlin_collections_execute_and_recover_object_graph() -> Result<()> {
    let Some(wasm_path) = build_kotlin_collections_example()? else {
        println!("skipping kotlin collections execution test: gradle is unavailable");
        return Ok(());
    };

    let sidecar = parse_kotlin_sidecar(
        std::fs::read("examples/transaction-kotlin/collections/twasm.kotlin.json")?.as_slice(),
    )?;
    let input = std::fs::read(&wasm_path)?;
    let (rewritten, report) = rewrite_kotlin_module(&input, &sidecar)?;
    ensure!(report.persistent_types > 1, "generic mode should discover runtime layouts");

    let temp = tempfile::tempdir()?;
    let tmemory_path = temp.path().join("kotlin-collections.tmemory");
    let tx_log_path = temp.path().join("kotlin-collections.txlog");

    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, &rewritten)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |ctx| ctx)?;
    let mut store = Store::new(&engine, wasmtime_wasi::WasiCtxBuilder::new().build_p1());
    create_file_backed_storage_for_test(&mut store, tmemory_path, tx_log_path.clone(), 64)?;

    let instance = linker.instantiate(&mut store, &module)?;
    let module_fingerprint = module_fingerprint_bytes_for_test(&rewritten);
    register_module_defined_durable_func_refs_with_fingerprint_for_test(
        &mut store,
        &instance,
        module_fingerprint,
    )?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    start.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    ensure!(recovered.root_object_ids.len() == 1, "expected one Profile root");
    ensure!(
        recovered.object_winners.len() >= 6,
        "expected Profile plus String/List/Map/backing object records, got {}",
        recovered.object_winners.len()
    );
    Ok(())
}
```

- [ ] **Step 2: Verify the test fails before Tasks 2-3 are complete**

Run:

```bash
cargo test --features transaction --test transaction_kotlin_collections -- --format terse
```

Expected before lowerer work: failure from unresolved runtime type names or
ordinary `array.set`/`struct.set` inside helper functions.

- [ ] **Step 3: Verify the test passes after Tasks 2-4**

Run:

```bash
cargo test --features transaction --test transaction_kotlin_collections -- --format terse
```

Expected: pass, or skip only when Gradle is unavailable.

- [ ] **Step 4: Commit**

```bash
git add tests/transaction_kotlin_collections.rs
git commit -m "Recover Kotlin generic collection object graphs"
```

---

## Task 6: Docs And Full Verification

**Files:**
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-design.md`
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`

- [ ] **Step 1: Update design doc**

Replace the old first-wave exclusion of strings/collections with:

```markdown
Generic WasmGC graph mode supports Kotlin `String` and standard collection
object graphs by capturing their actual WasmGC struct/array layouts. This mode
requires the same Kotlin/Wasm module layout at recovery time and is intentionally
module-version-specific. Container-specific durable formats are deferred.
```

- [ ] **Step 2: Update implementation log**

Add:

```markdown
## 2026-06-17: Generic Kotlin WasmGC Graph Persistence

- Added sidecar opt-in for generic module-wide WasmGC graph capture.
- The Kotlin lowerer rewrites transaction call closures, not just explicitly
  named transaction functions.
- Real Kotlin `String`, `MutableList`, and `MutableMap` graphs now execute,
  commit, and recover through the file-backed persistent object path.
```

- [ ] **Step 3: Run full verification**

Run:

```bash
cargo fmt --all --check
cargo test -p wasmtime-transaction-tools -- --format terse
cargo test --features transaction --test transaction_kotlin_toolset -- --format terse
cargo test --features transaction --test transaction_kotlin_collections -- --format terse
cargo test --features transaction -p wasmtime --test transaction_persistence -- --format terse
git diff --check
```

Expected:

- all tests pass,
- Kotlin collections test skips only if Gradle is unavailable,
- no whitespace errors.

- [ ] **Step 4: Commit docs and verification fixes**

```bash
git add docs/shisoft/2026-06-17-kotlin-object-persistence-design.md \
        docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md
git commit -m "Document Kotlin generic WasmGC persistence"
```

---

## Self-Review Notes

- The plan avoids per-container durable encodings.
- The sidecar opt-in keeps old Kotlin and hand-WAT tests stable.
- The existing `StoreBackedOrdinaryGcPromotionAdapter` already snapshots
  ordinary WasmGC structs/arrays, i31s, durable funcrefs, and durable externrefs.
- The main new risk is transaction mutation coverage inside Kotlin stdlib helper
  functions; Task 3 addresses this through call-closure rewriting.
- Denylist policy is intentionally small and conservative. Unsupported runtime
  objects can still fail promotion with the existing runtime errors.
