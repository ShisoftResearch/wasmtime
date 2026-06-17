# Kotlin Object Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a tracked Kotlin/Wasm object-memory toolchain that lowers a small annotated Kotlin/WasmGC program into real transactional Wasm object operations and validates commit, recovery, promotion, and persistent GC.

**Architecture:** Start with a tracked Kotlin/Wasm example and checked-in sidecar metadata, then extend `wasmtime-transaction-tools` with Kotlin-specific inspect/build/rewrite commands. The first runtime integration uses ordinary Kotlin/WasmGC allocation for `struct.new`/`array.new`, then promotes transaction-reachable `@Persistent` objects into `ObjectId` persistent records before commit.

**Tech Stack:** Kotlin/Wasm Gradle plugin `2.4.0`, WasmGC/WASI, Rust `wasmparser`/`wasm-encoder`/`wasmprinter`, existing `wasmtime-transaction-tools`, Wasmtime `transaction` feature, file-backed transaction persistence tests.

**Current status, 2026-06-17:** Tasks 1 through 8 are implemented for the first
tracked Kotlin object-memory path. The lowerer now consumes both inline root
markers and explicit `twasm.root.get`/`twasm.root.set` marker imports, strips
those marker imports, remaps function indices, and verifies the resulting module
against the existing transaction Wasmtime runtime. Remaining Kotlin-language
work is SDK/compiler polish and broader language examples, not a second runtime
promotion path.

---

## Scope

This plan implements the approved object-memory design in
`docs/shisoft/2026-06-17-kotlin-object-persistence-design.md`.

First-wave support:

- `@Persistent` classes with primitive fields and persistent references.
- `@TxnFunc` transaction functions.
- `transaction { ... }` transaction boundary marker.
- `root<T>("name")` durable root marker.
- `setRoot("name", value)` durable root assignment marker.
- ordinary Kotlin/WasmGC `struct.new`/`array.new` allocation first.
- transaction-time promotion to durable `ObjectId` records only when persistent reachability requires it.
- simple-transactions compatibility checks for real `tfunc`/`tcall`/`tstruct`/`tarray` output.

Explicitly out of scope:

- Kotlin unsafe linear memory persistence.
- Kotlin collections and strings as persistent user types.
- Kotlin compiler plugin.
- Browser/Compose support.
- Multi-module user projects.

## File Structure

Create:

- `examples/transaction-kotlin/settings.gradle.kts`
  - Root Gradle project for tracked Kotlin examples.
- `examples/transaction-kotlin/build.gradle.kts`
  - Kotlin plugin version and shared repositories.
- `examples/transaction-kotlin/sdk/build.gradle.kts`
  - Kotlin SDK library module.
- `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`
  - Annotation and marker API.
- `examples/transaction-kotlin/bank/build.gradle.kts`
  - Kotlin/Wasm WASI executable demo.
- `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`
  - Bank/account demo using object memory only.
- `examples/transaction-kotlin/bank/twasm.kotlin.json`
  - First checked-in metadata sidecar.
- `crates/transaction-tools/src/kotlin_metadata.rs`
  - Sidecar metadata model and validation.
- `crates/transaction-tools/src/kotlin_inspect.rs`
  - Kotlin/WasmGC module inspection report.
- `crates/transaction-tools/src/kotlin_rewrite.rs`
  - Kotlin lowering entry point and report.
- `crates/transaction-tools/src/kotlin_cli.rs`
  - `twasm-kotlin` command.
- `crates/transaction-tools/src/kotlin_main.rs`
  - Binary entry point.
- `crates/transaction-tools/tests/kotlin_metadata.rs`
  - Sidecar parser unit tests.
- `crates/transaction-tools/tests/kotlin_inspect.rs`
  - Hand-WAT WasmGC inspection tests.
- `crates/transaction-tools/tests/kotlin_rewrite.rs`
  - Hand-WAT lowering shape tests.
- `tests/transaction_kotlin_toolset.rs`
  - Gradle build, lowerer, simple-transaction shape, and execution tests.
- `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`
  - Short running log for this toolchain.

Modify:

- `crates/transaction-tools/Cargo.toml`
  - Add `twasm-kotlin` binary. Existing dependencies cover the planned tests.
- `crates/transaction-tools/src/lib.rs`
  - Export Kotlin metadata/inspect/rewrite modules.
- Existing Wasmtime runtime promotion code is reused as-is.
  - `StoreBackedOrdinaryGcPromotionAdapter` in `crates/wasmtime/src/runtime/vm/libcalls.rs`
    already snapshots ordinary WasmGC structs/arrays into persistent `ObjectId`
    records before commit.
  - Kotlin work must only generate/lower Wasm into that existing transaction
    boundary and verify it with Kotlin-shaped modules.

---

## Task 1: Track A Minimal Kotlin/Wasm Object-Memory Example

**Files:**

- Create: `examples/transaction-kotlin/settings.gradle.kts`
- Create: `examples/transaction-kotlin/build.gradle.kts`
- Create: `examples/transaction-kotlin/sdk/build.gradle.kts`
- Create: `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`
- Create: `examples/transaction-kotlin/bank/build.gradle.kts`
- Create: `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`
- Create: `examples/transaction-kotlin/bank/twasm.kotlin.json`
- Create: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Add the Gradle root**

Create `examples/transaction-kotlin/settings.gradle.kts`:

```kotlin
pluginManagement {
    repositories {
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositories {
        mavenCentral()
    }
}

rootProject.name = "transaction-kotlin"
include(":sdk")
include(":bank")
```

Create `examples/transaction-kotlin/build.gradle.kts`:

```kotlin
plugins {
    kotlin("multiplatform") version "2.4.0" apply false
}
```

- [ ] **Step 2: Add the Kotlin SDK marker library**

Create `examples/transaction-kotlin/sdk/build.gradle.kts`:

```kotlin
plugins {
    kotlin("multiplatform")
}

kotlin {
    wasmWasi()
}
```

Create `examples/transaction-kotlin/sdk/src/commonMain/kotlin/twasm/Transaction.kt`:

```kotlin
package twasm

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class Persistent

@Target(AnnotationTarget.FUNCTION)
@Retention(AnnotationRetention.BINARY)
annotation class TxnFunc

inline fun transaction(block: () -> Unit) {
    block()
}

@Suppress("UNUSED_PARAMETER")
inline fun <reified T> root(name: String): T {
    error("twasm root marker was not lowered: $name")
}

@Suppress("UNUSED_PARAMETER")
inline fun <reified T> setRoot(name: String, value: T) {
    error("twasm setRoot marker was not lowered: $name")
}
```

- [ ] **Step 3: Add the Kotlin bank example**

Create `examples/transaction-kotlin/bank/build.gradle.kts`:

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

Create `examples/transaction-kotlin/bank/src/wasmWasiMain/kotlin/Main.kt`:

```kotlin
import twasm.Persistent
import twasm.TxnFunc
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long) {
    val bank = Bank(Account(alice), Account(bob))
    setRoot("bank", bank)
}

fun main() {
    transaction {
        val bank = root<Bank>("bank")
        transfer(bank, true, 100)
    }
}
```

- [ ] **Step 4: Add the first sidecar metadata file**

Create `examples/transaction-kotlin/bank/twasm.kotlin.json`:

```json
{
  "version": 1,
  "module": "transaction-kotlin-bank",
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
        { "name": "bob", "kind": "ref", "type": "Account", "nullable": true }
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

For this first sidecar, `nullable` means the emitted WasmGC storage
nullability that the lowerer validates. It does not yet encode Kotlin
source-level non-null guarantees.

- [ ] **Step 5: Add a failing Gradle build test**

Create `tests/transaction_kotlin_toolset.rs`:

```rust
#![cfg(all(feature = "transaction", unix))]

use std::path::Path;
use std::process::Command;
use wasmtime::anyhow::{Context, Result, bail};

fn gradle_available() -> bool {
    Command::new("gradle")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn kotlin_wasm_path() -> &'static Path {
    Path::new("examples/transaction-kotlin")
}

#[test]
fn kotlin_bank_example_builds_to_wasm_wasi() -> Result<()> {
    if !gradle_available() {
        eprintln!("skipping Kotlin/Wasm test because gradle is not on PATH");
        return Ok(());
    }

    let status = Command::new("gradle")
        .arg(":bank:compileDevelopmentExecutableKotlinWasmWasi")
        .current_dir(kotlin_wasm_path())
        .status()
        .context("failed to run gradle for Kotlin/Wasm bank example")?;

    if !status.success() {
        bail!("Kotlin/Wasm bank build failed with {status}");
    }

    let wasm = kotlin_wasm_path()
        .join("bank/build/compileSync/wasmWasi/main/developmentExecutable/kotlin/bank.wasm");
    assert!(wasm.exists(), "missing Kotlin/Wasm artifact: {}", wasm.display());
    Ok(())
}
```

- [ ] **Step 6: Run the failing test**

Run:

```bash
cargo test --test transaction_kotlin_toolset kotlin_bank_example_builds_to_wasm_wasi -- --format terse
```

Expected before files are complete: FAIL because the Kotlin project is missing or does not build.

- [ ] **Step 7: Run the passing test**

Run:

```bash
cargo test --test transaction_kotlin_toolset kotlin_bank_example_builds_to_wasm_wasi -- --format terse
```

Expected after files are complete: PASS, or SKIP with a printed message only if Gradle is not installed.

- [ ] **Step 8: Commit**

```bash
git add examples/transaction-kotlin tests/transaction_kotlin_toolset.rs
git commit -m "Add tracked Kotlin transaction example"
```

---

## Task 2: Add Kotlin Metadata Parsing And Validation

**Files:**

- Create: `crates/transaction-tools/src/kotlin_metadata.rs`
- Create: `crates/transaction-tools/tests/kotlin_metadata.rs`
- Modify: `crates/transaction-tools/src/lib.rs`
- Modify: `crates/transaction-tools/Cargo.toml`

- [ ] **Step 1: Write metadata parser tests**

Create `crates/transaction-tools/tests/kotlin_metadata.rs`:

```rust
use wasmtime_transaction_tools::kotlin_metadata::{
    KotlinFieldKind, KotlinPersistentKind, KotlinSidecar, parse_kotlin_sidecar,
};

#[test]
fn parses_bank_sidecar() {
    let json = r#"
    {
      "version": 1,
      "module": "transaction-kotlin-bank",
      "persistentTypes": [
        {
          "name": "Account",
          "kind": "struct",
          "fields": [
            { "name": "balance", "kind": "i64", "nullable": false }
          ]
        }
      ],
      "transactionFunctions": ["transfer"],
      "roots": [
        { "name": "bank", "type": "Account", "nullable": false }
      ]
    }
    "#;

    let sidecar = parse_kotlin_sidecar(json.as_bytes()).unwrap();
    assert_eq!(sidecar.version, 1);
    assert_eq!(sidecar.module, "transaction-kotlin-bank");
    assert_eq!(sidecar.persistent_types[0].kind, KotlinPersistentKind::Struct);
    assert_eq!(sidecar.persistent_types[0].fields[0].kind, KotlinFieldKind::I64);
}

#[test]
fn rejects_duplicate_persistent_type_names() {
    let json = r#"
    {
      "version": 1,
      "module": "bad",
      "persistentTypes": [
        { "name": "Account", "kind": "struct", "fields": [] },
        { "name": "Account", "kind": "struct", "fields": [] }
      ],
      "transactionFunctions": [],
      "roots": []
    }
    "#;

    let err = parse_kotlin_sidecar(json.as_bytes()).unwrap_err().to_string();
    assert!(err.contains("duplicate persistent type name"));
}

#[test]
fn rejects_unknown_root_type() {
    let json = r#"
    {
      "version": 1,
      "module": "bad",
      "persistentTypes": [],
      "transactionFunctions": [],
      "roots": [
        { "name": "bank", "type": "Bank", "nullable": false }
      ]
    }
    "#;

    let err = parse_kotlin_sidecar(json.as_bytes()).unwrap_err().to_string();
    assert!(err.contains("unknown root type"));
}
```

- [ ] **Step 2: Run tests and verify failure**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_metadata -- --format terse
```

Expected: FAIL because `kotlin_metadata` does not exist.

- [ ] **Step 3: Implement metadata model**

Create `crates/transaction-tools/src/kotlin_metadata.rs`:

```rust
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KotlinSidecar {
    pub version: u32,
    pub module: String,
    pub persistent_types: Vec<KotlinPersistentType>,
    pub transaction_functions: Vec<String>,
    pub roots: Vec<KotlinRoot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KotlinPersistentType {
    pub name: String,
    pub kind: KotlinPersistentKind,
    #[serde(default)]
    pub fields: Vec<KotlinField>,
    #[serde(default)]
    pub element: Option<KotlinField>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinPersistentKind {
    Struct,
    Array,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KotlinField {
    pub name: String,
    pub kind: KotlinFieldKind,
    #[serde(default)]
    pub r#type: Option<String>,
    pub nullable: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinFieldKind {
    I31,
    I32,
    I64,
    F32,
    F64,
    V128,
    Ref,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KotlinRoot {
    pub name: String,
    pub r#type: String,
    pub nullable: bool,
}

pub fn parse_kotlin_sidecar(mut bytes: impl Read) -> Result<KotlinSidecar> {
    let mut json = Vec::new();
    bytes
        .read_to_end(&mut json)
        .context("failed to read Kotlin sidecar")?;
    let sidecar: KotlinSidecar =
        serde_json::from_slice(&json).context("failed to parse Kotlin sidecar JSON")?;
    validate_kotlin_sidecar(&sidecar)?;
    Ok(sidecar)
}

pub fn validate_kotlin_sidecar(sidecar: &KotlinSidecar) -> Result<()> {
    if sidecar.version != 1 {
        bail!("unsupported Kotlin sidecar version {}", sidecar.version);
    }

    let mut type_names = BTreeSet::new();
    for ty in &sidecar.persistent_types {
        if ty.name.is_empty() {
            bail!("persistent type name cannot be empty");
        }
        if !type_names.insert(ty.name.as_str()) {
            bail!("duplicate persistent type name {}", ty.name);
        }
    }

    for ty in &sidecar.persistent_types {
        match ty.kind {
            KotlinPersistentKind::Struct => {
                if ty.element.is_some() {
                    bail!("struct type {} cannot have an array element layout", ty.name);
                }
            }
            KotlinPersistentKind::Array => {
                if ty.fields.len() > 0 {
                    bail!("array type {} cannot have struct fields", ty.name);
                }
                if ty.element.is_none() {
                    bail!("array type {} must have an element layout", ty.name);
                }
            }
        }
        for field in &ty.fields {
            validate_field(field, &type_names)?;
        }
        if let Some(element) = &ty.element {
            validate_field(element, &type_names)?;
        }
    }

    for root in &sidecar.roots {
        if !type_names.contains(root.r#type.as_str()) {
            bail!("unknown root type {} for root {}", root.r#type, root.name);
        }
    }

    Ok(())
}

fn validate_field(field: &KotlinField, known_types: &BTreeSet<&str>) -> Result<()> {
    if field.kind == KotlinFieldKind::Ref {
        let Some(target) = field.r#type.as_deref() else {
            bail!("ref field {} must name a target type", field.name);
        };
        if !known_types.contains(target) {
            bail!("unknown ref field target type {target} for field {}", field.name);
        }
    }
    Ok(())
}
```

Modify `crates/transaction-tools/src/lib.rs`:

```rust
pub mod cli;
pub mod kotlin_metadata;
pub mod metadata;
pub mod rewrite;

pub use kotlin_metadata::{KotlinSidecar, parse_kotlin_sidecar};
pub use metadata::{MetadataReport, inspect_module};
pub use rewrite::{RewriteReport, rewrite_module};
```

- [ ] **Step 4: Run metadata tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_metadata -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools/src/lib.rs crates/transaction-tools/src/kotlin_metadata.rs crates/transaction-tools/tests/kotlin_metadata.rs
git commit -m "Parse Kotlin transaction metadata sidecars"
```

---

## Task 3: Add `twasm-kotlin inspect`

**Files:**

- Create: `crates/transaction-tools/src/kotlin_cli.rs`
- Create: `crates/transaction-tools/src/kotlin_main.rs`
- Create: `crates/transaction-tools/src/kotlin_inspect.rs`
- Create: `crates/transaction-tools/tests/kotlin_inspect.rs`
- Modify: `crates/transaction-tools/Cargo.toml`
- Modify: `crates/transaction-tools/src/lib.rs`
- Modify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Add failing CLI tests**

Create `crates/transaction-tools/tests/kotlin_inspect.rs`:

```rust
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

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"persistent_types\": 1"));
    assert!(stdout.contains("\"transaction_functions\": 1"));
    assert!(stdout.contains("\"roots\": 1"));
}
```

- [ ] **Step 2: Run failing test**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_inspect -- --format terse
```

Expected: FAIL because `twasm-kotlin` does not exist.

- [ ] **Step 3: Add the binary and inspect implementation**

Modify `crates/transaction-tools/Cargo.toml`:

```toml
[[bin]]
name = "twasm-rust"
path = "src/main.rs"

[[bin]]
name = "twasm-kotlin"
path = "src/kotlin_main.rs"
```

Create `crates/transaction-tools/src/kotlin_main.rs`:

```rust
fn main() -> anyhow::Result<()> {
    wasmtime_transaction_tools::kotlin_cli::main()
}
```

Create `crates/transaction-tools/src/kotlin_inspect.rs`:

```rust
use serde::Serialize;

use crate::kotlin_metadata::KotlinSidecar;

#[derive(Debug, Serialize)]
pub struct KotlinInspectReport {
    pub module: String,
    pub persistent_types: usize,
    pub transaction_functions: usize,
    pub roots: usize,
}

pub fn inspect_sidecar(sidecar: &KotlinSidecar) -> KotlinInspectReport {
    KotlinInspectReport {
        module: sidecar.module.clone(),
        persistent_types: sidecar.persistent_types.len(),
        transaction_functions: sidecar.transaction_functions.len(),
        roots: sidecar.roots.len(),
    }
}
```

Create `crates/transaction-tools/src/kotlin_cli.rs`:

```rust
use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "twasm-kotlin")]
#[command(about = "Build, inspect, and lower Kotlin/Wasm transactional object modules")]
pub struct KotlinCli {
    #[command(subcommand)]
    command: KotlinCommand,
}

#[derive(Subcommand)]
enum KotlinCommand {
    Inspect {
        #[arg(long)]
        metadata: PathBuf,
    },
}

pub fn main() -> Result<()> {
    let mut stdout = std::io::stdout();
    run(KotlinCli::parse(), &mut stdout)
}

pub fn run(cli: KotlinCli, stdout: &mut impl Write) -> Result<()> {
    match cli.command {
        KotlinCommand::Inspect { metadata } => {
            let bytes = fs::read(&metadata)
                .with_context(|| format!("failed to read {}", metadata.display()))?;
            let sidecar = crate::kotlin_metadata::parse_kotlin_sidecar(bytes.as_slice())?;
            let report = crate::kotlin_inspect::inspect_sidecar(&sidecar);
            writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)?;
        }
    }
    Ok(())
}
```

Modify `crates/transaction-tools/src/lib.rs`:

```rust
pub mod cli;
pub mod kotlin_cli;
pub mod kotlin_inspect;
pub mod kotlin_metadata;
pub mod metadata;
pub mod rewrite;
```

- [ ] **Step 4: Run CLI tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_inspect -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Add Gradle build command test hook**

Extend `tests/transaction_kotlin_toolset.rs` with:

```rust
#[test]
fn twasm_kotlin_inspects_tracked_bank_sidecar() -> Result<()> {
    let output = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-kotlin",
            "--",
            "inspect",
            "--metadata",
            "examples/transaction-kotlin/bank/twasm.kotlin.json",
        ])
        .output()
        .context("failed to run twasm-kotlin inspect")?;

    if !output.status.success() {
        bail!(
            "twasm-kotlin inspect failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("\"persistent_types\": 2"));
    assert!(stdout.contains("\"transaction_functions\": 2"));
    assert!(stdout.contains("\"roots\": 1"));
    Ok(())
}
```

- [ ] **Step 6: Run integration test**

Run:

```bash
cargo test --test transaction_kotlin_toolset twasm_kotlin_inspects_tracked_bank_sidecar -- --format terse
```

Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/transaction-tools tests/transaction_kotlin_toolset.rs
git commit -m "Add Kotlin transaction tool inspection"
```

---

## Task 4: Discover Kotlin/WasmGC Shape From The Tracked Example

**Files:**

- Modify: `crates/transaction-tools/src/kotlin_inspect.rs`
- Modify: `crates/transaction-tools/src/kotlin_cli.rs`
- Create: `crates/transaction-tools/tests/kotlin_wasm_shape.rs`
- Modify: `tests/transaction_kotlin_toolset.rs`
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`

- [ ] **Step 1: Add Wasm shape report types**

Extend `crates/transaction-tools/src/kotlin_inspect.rs`:

```rust
#[derive(Debug, Default, Serialize)]
pub struct KotlinWasmShapeReport {
    pub functions: usize,
    pub gc_struct_types: usize,
    pub gc_array_types: usize,
    pub struct_new_ops: usize,
    pub array_new_ops: usize,
    pub struct_get_ops: usize,
    pub struct_set_ops: usize,
    pub array_get_ops: usize,
    pub array_set_ops: usize,
    pub marker_imports: usize,
}
```

Implement `inspect_wasm_shape(bytes: &[u8]) -> anyhow::Result<KotlinWasmShapeReport>` by scanning `wasmparser::Parser::new(0).parse_all(bytes)`, incrementing:

- `functions` from the function section count.
- `gc_struct_types` and `gc_array_types` from recursive type entries.
- `struct_new_ops`, `array_new_ops`, `struct_get_ops`, `struct_set_ops`, `array_get_ops`, `array_set_ops` from code-section operators.
- `marker_imports` from imports whose module starts with `twasm`.

- [ ] **Step 2: Add hand-WAT shape tests**

Create `crates/transaction-tools/tests/kotlin_wasm_shape.rs`:

```rust
use wasmtime_transaction_tools::kotlin_inspect::inspect_wasm_shape;

#[test]
fn shape_report_counts_wasmgc_struct_and_array_ops() {
    let wat = r#"
        (module
          (type $account (struct (field (mut i64))))
          (type $accounts (array (mut (ref null $account))))
          (func (export "make") (result (ref $account))
            i64.const 1
            struct.new $account)
          (func (export "read") (param (ref $account)) (result i64)
            local.get 0
            struct.get $account 0)
          (func (export "write") (param (ref $account))
            local.get 0
            i64.const 2
            struct.set $account 0)
          (func (export "array") (result (ref $accounts))
            ref.null $account
            i32.const 2
            array.new $accounts))
    "#;
    let wasm = wat::parse_str(wat).unwrap();
    let report = inspect_wasm_shape(&wasm).unwrap();

    assert_eq!(report.gc_struct_types, 1);
    assert_eq!(report.gc_array_types, 1);
    assert_eq!(report.struct_new_ops, 1);
    assert_eq!(report.array_new_ops, 1);
    assert_eq!(report.struct_get_ops, 1);
    assert_eq!(report.struct_set_ops, 1);
}
```

- [ ] **Step 3: Run failing shape tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_wasm_shape -- --format terse
```

Expected: FAIL until `inspect_wasm_shape` exists.

- [ ] **Step 4: Add `twasm-kotlin inspect --wasm`**

Extend `KotlinCommand::Inspect`:

```rust
Inspect {
    #[arg(long)]
    metadata: PathBuf,
    #[arg(long)]
    wasm: Option<PathBuf>,
}
```

When `--wasm` is present, print a JSON object containing both sidecar and Wasm shape reports.

- [ ] **Step 5: Run tool tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_wasm_shape -- --format terse
cargo test -p wasmtime-transaction-tools --test kotlin_inspect -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Run tracked Kotlin shape discovery**

Run:

```bash
gradle :bank:compileDevelopmentExecutableKotlinWasmWasi -p examples/transaction-kotlin
cargo run -p wasmtime-transaction-tools --bin twasm-kotlin -- inspect \
  --metadata examples/transaction-kotlin/bank/twasm.kotlin.json \
  --wasm examples/transaction-kotlin/bank/build/compileSync/wasmWasi/main/developmentExecutable/kotlin/bank.wasm
```

Expected: JSON reports at least one GC struct type and at least one `struct.new` operator.

- [ ] **Step 7: Log the discovered shape**

Create `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`:

```markdown
# Kotlin Object Persistence Implementation Log

## 2026-06-17: Kotlin/WasmGC Shape Discovery

- Built `examples/transaction-kotlin/bank` with Kotlin/Wasm.
- `twasm-kotlin inspect` validates `twasm.kotlin.json`.
- The emitted module contains WasmGC struct/array operations that the Kotlin
  lowerer can target.
- Kotlin object persistence remains object-memory only; unsafe linear memory is
  not part of this wave.
```

- [ ] **Step 8: Commit**

```bash
git add crates/transaction-tools tests/transaction_kotlin_toolset.rs docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md
git commit -m "Inspect Kotlin WasmGC transaction shape"
```

---

## Task 5: Add Kotlin Rewrite Report And Real Transaction Shape Gate

**Files:**

- Create: `crates/transaction-tools/src/kotlin_rewrite.rs`
- Create: `crates/transaction-tools/tests/kotlin_rewrite.rs`
- Modify: `crates/transaction-tools/src/kotlin_cli.rs`
- Modify: `crates/transaction-tools/src/lib.rs`
- Modify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Add failing rewrite tests**

Create `crates/transaction-tools/tests/kotlin_rewrite.rs`:

```rust
use wasmtime_transaction_tools::kotlin_metadata::parse_kotlin_sidecar;
use wasmtime_transaction_tools::kotlin_rewrite::rewrite_kotlin_module;

#[test]
fn rewrite_report_rejects_missing_transaction_function() {
    let wasm = wat::parse_str(
        r#"
        (module
          (func (export "not_transfer")))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
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
        }"#
        .as_bytes(),
    )
    .unwrap();

    let err = rewrite_kotlin_module(&wasm, &sidecar).unwrap_err().to_string();
    assert!(err.contains("transaction function transfer was not found"));
}

#[test]
fn rewrite_report_records_transaction_function_names() {
    let wasm = wat::parse_str(
        r#"
        (module
          (func $transfer (export "transfer")))
        "#,
    )
    .unwrap();
    let sidecar = parse_kotlin_sidecar(
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
        }"#
        .as_bytes(),
    )
    .unwrap();

    let (_wasm, report) = rewrite_kotlin_module(&wasm, &sidecar).unwrap();
    assert_eq!(report.transaction_functions, vec!["transfer"]);
}
```

- [ ] **Step 2: Run failing tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --format terse
```

Expected: FAIL because `kotlin_rewrite` does not exist.

- [ ] **Step 3: Implement minimal rewrite report**

Create `crates/transaction-tools/src/kotlin_rewrite.rs`:

```rust
use anyhow::{Context, Result, bail};
use serde::Serialize;
use wasmparser::{ExternalKind, Parser, Payload};

use crate::kotlin_metadata::KotlinSidecar;

#[derive(Debug, Default, Serialize)]
pub struct KotlinRewriteReport {
    pub transaction_functions: Vec<String>,
    pub persistent_types: usize,
    pub roots: usize,
    pub rewritten_tfuncs: usize,
    pub rewritten_object_ops: usize,
}

pub fn rewrite_kotlin_module(input: &[u8], sidecar: &KotlinSidecar) -> Result<(Vec<u8>, KotlinRewriteReport)> {
    let exports = exported_function_names(input)?;
    for name in &sidecar.transaction_functions {
        if !exports.iter().any(|export| export == name) {
            bail!("transaction function {name} was not found in module exports");
        }
    }

    Ok((
        input.to_vec(),
        KotlinRewriteReport {
            transaction_functions: sidecar.transaction_functions.clone(),
            persistent_types: sidecar.persistent_types.len(),
            roots: sidecar.roots.len(),
            rewritten_tfuncs: 0,
            rewritten_object_ops: 0,
        },
    ))
}

fn exported_function_names(input: &[u8]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for payload in Parser::new(0).parse_all(input) {
        if let Payload::ExportSection(exports) = payload.context("failed to parse wasm payload")? {
            for export in exports {
                let export = export.context("failed to parse export")?;
                if export.kind == ExternalKind::Func {
                    names.push(export.name.to_owned());
                }
            }
        }
    }
    Ok(names)
}
```

Modify `crates/transaction-tools/src/lib.rs`:

```rust
pub mod kotlin_rewrite;
```

- [ ] **Step 4: Add `twasm-kotlin rewrite`**

Extend `crates/transaction-tools/src/kotlin_cli.rs` with:

```rust
Rewrite {
    input: PathBuf,
    #[arg(long)]
    metadata: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long)]
    report: Option<PathBuf>,
}
```

Behavior:

- read input Wasm
- read sidecar metadata
- call `kotlin_rewrite::rewrite_kotlin_module`
- write output Wasm
- write report JSON if requested

- [ ] **Step 5: Run minimal rewrite tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Add a red integration test for real transaction ops**

Extend `tests/transaction_kotlin_toolset.rs`:

```rust
#[test]
fn kotlin_rewrite_output_must_contain_real_transaction_object_ops() -> Result<()> {
    if !gradle_available() {
        eprintln!("skipping Kotlin/Wasm test because gradle is not on PATH");
        return Ok(());
    }

    let build = Command::new("gradle")
        .arg(":bank:compileDevelopmentExecutableKotlinWasmWasi")
        .current_dir(kotlin_wasm_path())
        .status()
        .context("failed to build Kotlin/Wasm bank")?;
    if !build.success() {
        bail!("Kotlin/Wasm bank build failed with {build}");
    }

    let temp = tempfile::tempdir()?;
    let out = temp.path().join("bank.twasm.wasm");
    let input = kotlin_wasm_path()
        .join("bank/build/compileSync/wasmWasi/main/developmentExecutable/kotlin/bank.wasm");

    let status = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-kotlin",
            "--",
            "rewrite",
        ])
        .arg(&input)
        .args([
            "--metadata",
            "examples/transaction-kotlin/bank/twasm.kotlin.json",
            "--output",
        ])
        .arg(&out)
        .status()
        .context("failed to run twasm-kotlin rewrite")?;

    if !status.success() {
        bail!("twasm-kotlin rewrite failed with {status}");
    }

    let bytes = std::fs::read(&out)?;
    let wat = wasmprinter::print_bytes(&bytes)?;
    assert!(wat.contains("tfunc") || wat.contains(".tcall"), "lowered Kotlin module must contain transaction function/call syntax");
    assert!(wat.contains("tstruct") || wat.contains("tarray"), "lowered Kotlin module must contain transaction object syntax");
    assert!(!wat.contains("twasm root marker"), "SDK marker strings must not be the persistence boundary");
    Ok(())
}
```

Expected before full lowering: FAIL because minimal rewrite is pass-through.

- [ ] **Step 7: Commit the minimal rewrite infrastructure**

```bash
git add crates/transaction-tools tests/transaction_kotlin_toolset.rs
git commit -m "Add Kotlin transaction rewrite scaffolding"
```

Keep the red integration test ignored only if necessary, with a clear `#[ignore = "Kotlin object lowering not implemented yet"]` tag. Do not remove it.

---

## Task 6: Implement Kotlin Object Lowering For A Hand-WAT Subset

**Files:**

- Modify: `crates/transaction-tools/src/kotlin_rewrite.rs`
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`

- [ ] **Step 1: Add hand-WAT object lowering tests**

Extend `crates/transaction-tools/tests/kotlin_rewrite.rs` with a test whose input uses ordinary WasmGC `struct.new`, `struct.get`, and `struct.set`, and whose metadata marks the type/function persistent. The assertion must require printed output to contain:

```text
tfunc
tstruct.get
tstruct.set
```

Use a compact fixture:

```rust
#[test]
fn rewrite_lowers_marked_struct_ops_to_transaction_ops() {
    let wat = r#"
        (module
          (type $account (struct (field (mut i64))))
          (func $transfer (export "transfer") (param (ref $account)) (result i64)
            local.get 0
            struct.get $account 0))
    "#;
    let input = wat::parse_str(wat).unwrap();
    let sidecar = parse_kotlin_sidecar(
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
        }"#
        .as_bytes(),
    )
    .unwrap();

    let (output, report) = rewrite_kotlin_module(&input, &sidecar).unwrap();
    let printed = wasmprinter::print_bytes(&output).unwrap();

    assert!(report.rewritten_tfuncs > 0);
    assert!(report.rewritten_object_ops > 0);
    assert!(printed.contains("tfunc"));
    assert!(printed.contains("tstruct.get"));
}
```

- [ ] **Step 2: Run failing hand-WAT tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite rewrite_lowers_marked_struct_ops_to_transaction_ops -- --format terse
```

Expected: FAIL because the lowerer is still pass-through.

- [ ] **Step 3: Implement transaction object rewriting for hand-WAT**

Implementation constraints:

- Use `wasm_encoder::reencode` as in `crates/transaction-tools/src/rewrite.rs`.
- Preserve module sections not touched by the lowerer.
- Add `shisoft.transaction.objects` metadata for transaction functions.
- Rewrite only functions listed in `sidecar.transaction_functions`.
- Rewrite only struct/array type indices matched to sidecar persistent types.
- For constructor ops, keep ordinary `struct.new`/`array.new` unless the constructor result is directly stored into a persistent root/object. This preserves the approved volatile-first allocation rule.
- For field/array access on known persistent references, rewrite:
  - `struct.get` -> `tstruct.get`
  - `struct.set` -> `tstruct.set`
  - `array.get` -> `tarray.get`
  - `array.set` -> `tarray.set`
  - `array.len` -> `tarray.len`

- [ ] **Step 4: Run hand-WAT lowerer tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools/src/kotlin_rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs
git commit -m "Lower Kotlin WasmGC object ops to transactions"
```

---

## Task 7: Verify Kotlin Uses Existing Ordinary WasmGC Promotion

**Files:**

- Modify: `crates/transaction-tools/src/kotlin_rewrite.rs`
- Modify: `crates/transaction-tools/tests/kotlin_rewrite.rs`
- Modify: `tests/transaction_kotlin_toolset.rs`

- [ ] **Step 1: Assert the runtime boundary is existing Wasmtime promotion**

Do not add a second ordinary-WasmGC promotion adapter for Kotlin. The runtime
already has the generic path:

```bash
rg -n "StoreBackedOrdinaryGcPromotionAdapter|impl OrdinaryGcPromotionAdapter" \
  crates/wasmtime/src/runtime/vm/libcalls.rs
rg -n "real_tfunc_promotes_ordinary" crates/wasmtime/tests/transaction_persistence.rs
```

Expected: the store-backed adapter and ordinary promotion recovery tests exist.

- [ ] **Step 2: Keep Kotlin allocation ordinary until persistence is required**

In `crates/transaction-tools/tests/kotlin_rewrite.rs`, add shape tests proving
the Kotlin lowerer does not eagerly allocate persistent objects for every
`struct.new`/`array.new`.

Expected lowering shape:

- ordinary Kotlin constructors remain ordinary WasmGC allocation sites,
- assignments to persistent roots/fields/arrays cross the existing transaction
  persistence boundary,
- unsupported referenced ordinary objects still fail before commit rather than
  being written as durable `VMGcRef`s.

- [ ] **Step 3: Register Kotlin persistent type layouts**

Use the Kotlin sidecar metadata to connect Kotlin persistent classes/arrays to
the existing Wasmtime persistent type-layout registry. This is the Kotlin-specific
part: the runtime promotion adapter can only snapshot an ordinary WasmGC object
when its runtime type maps to a persistent layout id.

Add tests that reject:

- missing layout metadata for an `@Persistent` class,
- field count/type mismatches between sidecar metadata and discovered WasmGC
  type shape,
- persistent references to ordinary Kotlin runtime classes that are not
  `@Persistent`.

- [ ] **Step 4: Add Kotlin-shaped promotion/recovery test**

In `tests/transaction_kotlin_toolset.rs`, add an end-to-end test that builds a
small Kotlin module, lowers it, runs it against file-backed storage, reopens the
storage, and verifies promoted objects are recovered through `ObjectId` records.

The module should cover:

- ordinary `struct.new`/`array.new` inside a transaction,
- promotion through persistent root assignment,
- at least one nested persistent object reference,
- alias preservation when two fields reference the same ordinary object,
- recovery after reopening the file-backed backend.

- [ ] **Step 5: Run Kotlin promotion integration tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test kotlin_rewrite -- --format terse
cargo test --test transaction_kotlin_toolset kotlin_promotes_ordinary_wasmgc_objects -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/transaction-tools/src/kotlin_rewrite.rs crates/transaction-tools/tests/kotlin_rewrite.rs tests/transaction_kotlin_toolset.rs
git commit -m "Verify Kotlin WasmGC promotion integration"
```

---

## Task 8: End-To-End Kotlin Rewrite, Execution, Recovery, And GC

**Files:**

- Modify: `crates/transaction-tools/src/kotlin_rewrite.rs`
- Modify: `tests/transaction_kotlin_toolset.rs`
- Modify: `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`

- [ ] **Step 1: Unignore or add the end-to-end Kotlin lowerer test**

In `tests/transaction_kotlin_toolset.rs`, add or unignore:

```rust
#[test]
fn kotlin_bank_rewrite_executes_and_recovers_persistent_objects() -> Result<()> {
    // Build Kotlin/Wasm bank.
    // Rewrite with twasm-kotlin.
    // Instantiate with transaction-enabled Wasmtime.
    // Execute init/transfer exports or a main transaction.
    // Reopen file-backed storage.
    // Verify recovered root Bank and Account object records.
    Ok(())
}
```

The test must use file-backed transaction storage APIs already used by `crates/wasmtime/tests/transaction_persistence.rs` and `tests/transaction_rust_toolset.rs`.

- [ ] **Step 2: Add simple-transactions shape inspection**

Extend the same test file with:

```rust
#[derive(Default)]
struct KotlinTransactionShape {
    tfuncs: usize,
    tstruct_gets: usize,
    tstruct_sets: usize,
    tarray_gets: usize,
    tarray_sets: usize,
    marker_imports: usize,
}
```

Scan the rewritten Kotlin module with `wasmparser` and assert:

- `marker_imports == 0`
- `tfuncs > 0`
- `tstruct_gets + tstruct_sets > 0`

- [ ] **Step 3: Ensure generated Wasm validates and instantiates**

Use this config in the test:

```rust
let mut config = wasmtime::Config::new();
config
    .wasm_bulk_memory(true)
    .wasm_gc(true)
    .wasm_reference_types(true)
    .wasm_function_references(true)
    .wasm_tail_call(true);
let engine = wasmtime::Engine::new(&config)?;
let module = wasmtime::Module::from_file(&engine, &rewritten_wasm_path)?;
```

- [ ] **Step 4: Run Kotlin toolset tests**

Run:

```bash
cargo test --test transaction_kotlin_toolset -- --format terse
```

Expected: PASS on machines with Gradle; SKIP only the Gradle-dependent cases when Gradle is unavailable.

- [ ] **Step 5: Run simple-transactions WAST regression gate**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstruct.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tarray.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref.wast -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Run persistent GC regression gates**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib persistent_mark_sweep -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: PASS.

- [ ] **Step 7: Update implementation log**

Append to `docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md`:

```markdown
## 2026-06-17: Kotlin Object Persistence End-To-End Gate

- The tracked Kotlin bank example builds to Wasm/WASI.
- `twasm-kotlin` rewrites the module into real transaction object operations.
- The rewritten module passes simple-transactions shape checks.
- File-backed recovery reconstructs the persistent Kotlin object graph.
- Persistent GC keeps reachable Kotlin bank/account objects.
```

- [ ] **Step 8: Commit**

```bash
git add crates/transaction-tools tests/transaction_kotlin_toolset.rs docs/shisoft/2026-06-17-kotlin-object-persistence-implementation-log.md
git commit -m "Validate Kotlin persistent object toolchain"
```

---

## Task 9: Final Quality Gate

**Files:**

- Modify only files needed to fix issues found by verification.

- [ ] **Step 1: Run formatting**

Run:

```bash
cargo fmt --check
```

Expected: PASS. If it fails, run `cargo fmt`, inspect the diff, then continue.

- [ ] **Step 2: Run transaction-tools tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools -- --format terse
```

Expected: PASS.

- [ ] **Step 3: Run Kotlin integration tests**

Run:

```bash
cargo test --test transaction_kotlin_toolset -- --format terse
```

Expected: PASS on this machine because Gradle is installed.

- [ ] **Step 4: Run transaction runtime regression tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib persistent_mark_sweep -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Run simple-transactions object regression tests**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstruct.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tarray.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref.wast -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Run whitespace check**

Run:

```bash
git diff --check
```

Expected: PASS.

- [ ] **Step 7: Commit any verification fixes**

```bash
git status --short
git add -p
git commit -m "Fix Kotlin transaction toolchain verification issues"
```

Only commit if there are actual fixes.

---

## Self-Review

Spec coverage:

- Tracked Kotlin example: Task 1.
- Object memory only, no unsafe linear memory: Task 1 SDK/example and Task 8 tests.
- No Kotlin compiler fork: all tasks use Gradle output plus post-Wasm tooling.
- Sidecar metadata: Task 2.
- Lowerer/tooling: Tasks 3, 5, and 6.
- Real simple-transactions compatibility: Tasks 5 and 8.
- Transaction-time promotion: Task 7 verifies Kotlin-shaped modules use the
  existing Wasmtime ordinary-WasmGC-to-`ObjectId` promotion path.
- File-backed recovery and persistent GC: Task 8.
- Final verification: Task 9.

Plan hygiene:

- The plan uses exact file paths and avoids open-ended implementation slots.
- Runtime promotion is not duplicated in the Kotlin toolchain plan; only
  Kotlin metadata, lowering shape, and integration tests remain here.

Execution notes:

- Use one subagent per task when practical.
- Review each task diff before dispatching the next task.
- Commit after each task that leaves tests passing.
- Do not modify WAST test cases or upstream proposal fixtures.
