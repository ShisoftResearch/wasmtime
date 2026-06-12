# Rust Transaction Toolset Demo Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a reusable Rust-to-transactional-Wasm toolset that lets demo and pressure-test Rust programs use typed persistent roots, ordinary Rust field access, and a postpass that lowers marked code into transactional Wasm.

**Architecture:** The first backend represents persistent Rust objects as typed roots over `tmemory` byte ranges because file-backed recoverable `tmemory` already works. The SDK preserves the final persistent-identity shape with a packed `PersistentId`, where `TMemory` roots are implemented first and object IDs remain encoded but rejected by the postpass until transactional persistent GC/object storage is ready. The postpass reads SDK marker imports and metadata, rewrites marked functions into transactional functions, lowers persistent-address markers into addresses, and rewrites tainted loads/stores into transactional memory operations.

**Tech Stack:** Rust 2024, proc macros (`syn`, `quote`, `proc-macro2`), `wasmparser`, `wasm-encoder`, `wasmprinter`, `wat`, existing Wasmtime `transaction` feature, `wasm32-unknown-unknown` Rust examples, file-backed `tmemory` recovery tests.

---

## File Structure

- Modify `Cargo.toml`
  - Add workspace members for the SDK, macro crate, and tool crate.
  - Add workspace dependency aliases for the new crates.
- Create `crates/transaction-sdk/`
  - Guest-facing Rust SDK used by programs compiled to Wasm.
  - Defines `PersistentId`, `Root<T>`, `Tx<'tx>`, `PRef<'tx, T>`, `PMut<'tx, T>`, `Persist`, marker intrinsics, and `transaction!`.
- Create `crates/transaction-sdk-macros/`
  - Implements `#[derive(Persist)]` and `#[transaction]`.
  - Emits marker calls/custom-section metadata consumed by the postpass.
- Create `crates/transaction-tools/`
  - Implements the postpass library and CLI binary `twasm-rust`.
  - Provides `rewrite`, `inspect`, and `build` subcommands.
- Create `examples/transaction-rust/bank/`
  - First Rust guest fixture using the SDK.
  - Kept as one example among many, not as a special case in the toolchain.
- Create `examples/transaction-rust/pressure/`
  - Generic pressure fixture with configurable transfer loops over fixed-size persistent records.
- Create `tests/transaction_rust_toolset.rs`
  - Host-side integration tests for postpass behavior and end-to-end file-backed recovery.
- Create `tests/transaction_rust_simple_transactions.rs`
  - Verifies the Rust postpass output is real simple-transactions Wasm, not a parallel SDK-call ABI.
- Modify `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record that the Rust frontend is a toolchain layer over transactional Wasm, not a replacement for runtime semantics.

---

## Task 1: Add Toolset Crates To The Workspace

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/transaction-sdk/Cargo.toml`
- Create: `crates/transaction-sdk/src/lib.rs`
- Create: `crates/transaction-sdk-macros/Cargo.toml`
- Create: `crates/transaction-sdk-macros/src/lib.rs`
- Create: `crates/transaction-tools/Cargo.toml`
- Create: `crates/transaction-tools/src/lib.rs`
- Create: `crates/transaction-tools/src/main.rs`
- Create: `crates/transaction-tools/src/rewrite.rs`

- [ ] **Step 1: Add workspace members and dependency aliases**

Add the new crates to the `members` array in the root `Cargo.toml`:

```toml
  "crates/transaction-sdk",
  "crates/transaction-sdk-macros",
  "crates/transaction-tools",
```

Add these entries under `[workspace.dependencies]`:

```toml
wasmtime-transaction-sdk = { path = "crates/transaction-sdk", version = "=46.0.0" }
wasmtime-transaction-sdk-macros = { path = "crates/transaction-sdk-macros", version = "=46.0.0" }
wasmtime-transaction-tools = { path = "crates/transaction-tools", version = "=46.0.0" }
```

- [ ] **Step 2: Create the SDK crate manifest**

Create `crates/transaction-sdk/Cargo.toml`:

```toml
[package]
name = "wasmtime-transaction-sdk"
version.workspace = true
authors.workspace = true
edition.workspace = true
rust-version.workspace = true
license = "Apache-2.0 WITH LLVM-exception"

[lints]
workspace = true

[features]
default = ["macros"]
macros = ["dep:wasmtime-transaction-sdk-macros"]

[dependencies]
wasmtime-transaction-sdk-macros = { workspace = true, optional = true }
```

- [ ] **Step 3: Create the macro crate manifest**

Create `crates/transaction-sdk-macros/Cargo.toml`:

```toml
[package]
name = "wasmtime-transaction-sdk-macros"
version.workspace = true
authors.workspace = true
edition.workspace = true
rust-version.workspace = true
license = "Apache-2.0 WITH LLVM-exception"

[lib]
proc-macro = true

[lints]
workspace = true

[dependencies]
proc-macro2 = { workspace = true }
quote = { workspace = true }
syn = { workspace = true, features = ["full"] }
```

- [ ] **Step 4: Create the tool crate manifest**

Create `crates/transaction-tools/Cargo.toml`:

```toml
[package]
name = "wasmtime-transaction-tools"
version.workspace = true
authors.workspace = true
edition.workspace = true
rust-version.workspace = true
license = "Apache-2.0 WITH LLVM-exception"

[lints]
workspace = true

[[bin]]
name = "twasm-rust"
path = "src/main.rs"

[dependencies]
anyhow = { workspace = true, features = ["std"] }
clap = { workspace = true, features = ["derive"] }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true, features = ["std"] }
wasm-encoder = { workspace = true, features = ["wasmparser"] }
wasmparser = { workspace = true, features = ["validate", "features"] }
wasmprinter = { workspace = true }
wat = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 5: Add minimal crate roots**

Create `crates/transaction-sdk/src/lib.rs`:

```rust
#![cfg_attr(target_arch = "wasm32", no_std)]

#[cfg(feature = "macros")]
pub use wasmtime_transaction_sdk_macros::{transaction, Persist};

pub mod id;
pub mod marker;
pub mod persist;
pub mod root;
pub mod tx;

pub use id::{PersistentId, PersistentIdKind};
pub use persist::Persist;
pub use root::{PMut, PRef, Root};
pub use tx::Tx;

#[macro_export]
macro_rules! transaction {
    ($tx:ident, $body:block) => {{
        $crate::marker::mark_transaction_func();
        let $tx = unsafe { $crate::Tx::from_marker() };
        $body
    }};
}
```

Create `crates/transaction-sdk-macros/src/lib.rs`:

```rust
use proc_macro::TokenStream;

#[proc_macro_derive(Persist)]
pub fn derive_persist(input: TokenStream) -> TokenStream {
    input
}

#[proc_macro_attribute]
pub fn transaction(_args: TokenStream, input: TokenStream) -> TokenStream {
    input
}
```

Create `crates/transaction-tools/src/lib.rs`:

```rust
pub mod rewrite;

pub use rewrite::{RewriteReport, rewrite_module};
```

Create `crates/transaction-tools/src/main.rs`:

```rust
fn main() -> anyhow::Result<()> {
    wasmtime_transaction_tools::rewrite::main()
}
```

Create `crates/transaction-tools/src/rewrite.rs`:

```rust
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct RewriteReport {
    pub transaction_functions: usize,
    pub persistent_addr_markers: usize,
    pub i32_tloads: usize,
    pub i64_tloads: usize,
    pub f32_tloads: usize,
    pub f64_tloads: usize,
    pub i32_tstores: usize,
    pub i64_tstores: usize,
    pub f32_tstores: usize,
    pub f64_tstores: usize,
}

pub fn rewrite_module(input: &[u8]) -> Result<(Vec<u8>, RewriteReport)> {
    Ok((input.to_vec(), RewriteReport::default()))
}

pub fn main() -> anyhow::Result<()> {
    println!("twasm-rust rewrite support is not initialized until the CLI task");
    Ok(())
}
```

- [ ] **Step 6: Run skeleton checks**

Run:

```bash
cargo check -p wasmtime-transaction-sdk
cargo check -p wasmtime-transaction-sdk-macros
cargo check -p wasmtime-transaction-tools
```

Expected: all three commands compile, though the macros are pass-through and the tool rewrite is still a no-op.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/transaction-sdk crates/transaction-sdk-macros crates/transaction-tools
git commit -m "Add Rust transaction toolset crates"
```

---

## Task 2: Implement The SDK Core Types

**Files:**
- Create: `crates/transaction-sdk/src/id.rs`
- Create: `crates/transaction-sdk/src/persist.rs`
- Create: `crates/transaction-sdk/src/root.rs`
- Create: `crates/transaction-sdk/src/tx.rs`
- Create: `crates/transaction-sdk/src/marker.rs`
- Modify: `crates/transaction-sdk/src/lib.rs`

- [ ] **Step 1: Add ID tests first**

Create `crates/transaction-sdk/src/id.rs` with tests and stubs:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PersistentIdKind {
    TMemory = 0,
    Object = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct PersistentId(u64);

impl PersistentId {
    pub const KIND_BITS: u64 = 4;
    pub const PAYLOAD_BITS: u64 = 60;
    pub const PAYLOAD_MASK: u64 = (1u64 << Self::PAYLOAD_BITS) - 1;

    pub const fn tmemory_offset(offset: u32) -> Self {
        Self(((PersistentIdKind::TMemory as u64) << Self::PAYLOAD_BITS) | offset as u64)
    }

    pub const fn object_id(object_id: u64) -> Self {
        Self(((PersistentIdKind::Object as u64) << Self::PAYLOAD_BITS) | (object_id & Self::PAYLOAD_MASK))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn payload(self) -> u64 {
        self.0 & Self::PAYLOAD_MASK
    }

    pub const fn kind(self) -> PersistentIdKind {
        match self.0 >> Self::PAYLOAD_BITS {
            0 => PersistentIdKind::TMemory,
            1 => PersistentIdKind::Object,
            _ => PersistentIdKind::Object,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmemory_id_encodes_kind_and_offset() {
        let id = PersistentId::tmemory_offset(128);
        assert_eq!(id.kind(), PersistentIdKind::TMemory);
        assert_eq!(id.payload(), 128);
        assert_eq!(id.raw() & PersistentId::PAYLOAD_MASK, 128);
    }

    #[test]
    fn object_id_encodes_kind_and_payload() {
        let id = PersistentId::object_id(42);
        assert_eq!(id.kind(), PersistentIdKind::Object);
        assert_eq!(id.payload(), 42);
    }
}
```

- [ ] **Step 2: Run the ID tests**

Run:

```bash
cargo test -p wasmtime-transaction-sdk id::tests -- --format terse
```

Expected: PASS.

- [ ] **Step 3: Add `Persist`, root handles, and transaction token**

Create `crates/transaction-sdk/src/persist.rs`:

```rust
pub unsafe trait Persist: Sized + 'static {
    const TYPE_NAME: &'static str;
    const SIZE: usize = core::mem::size_of::<Self>();
    const ALIGN: usize = core::mem::align_of::<Self>();
}

unsafe impl Persist for u8 {
    const TYPE_NAME: &'static str = "u8";
}

unsafe impl Persist for u16 {
    const TYPE_NAME: &'static str = "u16";
}

unsafe impl Persist for u32 {
    const TYPE_NAME: &'static str = "u32";
}

unsafe impl Persist for u64 {
    const TYPE_NAME: &'static str = "u64";
}

unsafe impl Persist for i32 {
    const TYPE_NAME: &'static str = "i32";
}

unsafe impl Persist for i64 {
    const TYPE_NAME: &'static str = "i64";
}

unsafe impl Persist for f32 {
    const TYPE_NAME: &'static str = "f32";
}

unsafe impl Persist for f64 {
    const TYPE_NAME: &'static str = "f64";
}
```

Create `crates/transaction-sdk/src/marker.rs`:

```rust
use crate::PersistentId;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "twasm_intrinsics")]
unsafe extern "C" {
    #[link_name = "__twasm_persistent_addr_mut"]
    fn wasm_persistent_addr_mut(raw_id: u64) -> *mut u8;

    #[link_name = "__twasm_mark_transaction_func"]
    fn wasm_mark_transaction_func();

    #[link_name = "__twasm_mark_persistent_arg"]
    fn wasm_mark_persistent_arg(index: u32);
}

pub unsafe fn persistent_addr_mut(id: PersistentId) -> *mut u8 {
    #[cfg(target_arch = "wasm32")]
    {
        unsafe { wasm_persistent_addr_mut(id.raw()) }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        id.payload() as usize as *mut u8
    }
}

pub fn mark_transaction_func() {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        wasm_mark_transaction_func();
    }
}

pub fn mark_persistent_arg(index: u32) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        wasm_mark_persistent_arg(index);
    }

    #[cfg(not(target_arch = "wasm32"))]
    let _ = index;
}
```

Create `crates/transaction-sdk/src/tx.rs`:

```rust
use core::marker::PhantomData;

use crate::{PMut, PRef, Persist, Root};

pub struct Tx<'tx> {
    _marker: PhantomData<&'tx mut ()>,
}

impl<'tx> Tx<'tx> {
    pub unsafe fn from_marker() -> Self {
        Self { _marker: PhantomData }
    }

    pub fn root_ref<T: Persist>(&self, root: &Root<T>) -> PRef<'tx, T> {
        PRef::from_id(root.id())
    }

    pub fn root_mut<T: Persist>(&mut self, root: &Root<T>) -> PMut<'tx, T> {
        PMut::from_id(root.id())
    }
}
```

Create `crates/transaction-sdk/src/root.rs`:

```rust
use core::marker::PhantomData;

use crate::{marker, Persist, PersistentId};

#[derive(Clone, Copy)]
pub struct Root<T: Persist> {
    id: PersistentId,
    _marker: PhantomData<T>,
}

impl<T: Persist> Root<T> {
    pub const fn from_id(id: PersistentId) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub const fn tmemory_offset(offset: u32) -> Self {
        Self::from_id(PersistentId::tmemory_offset(offset))
    }

    pub const fn id(&self) -> PersistentId {
        self.id
    }
}

#[derive(Clone, Copy)]
pub struct PRef<'tx, T: Persist> {
    id: PersistentId,
    _marker: PhantomData<&'tx T>,
}

impl<'tx, T: Persist> PRef<'tx, T> {
    pub(crate) const fn from_id(id: PersistentId) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub fn as_ref(self) -> &'tx T {
        unsafe { &*(marker::persistent_addr_mut(self.id) as *const T) }
    }
}

pub struct PMut<'tx, T: Persist> {
    id: PersistentId,
    _marker: PhantomData<&'tx mut T>,
}

impl<'tx, T: Persist> PMut<'tx, T> {
    pub(crate) const fn from_id(id: PersistentId) -> Self {
        Self { id, _marker: PhantomData }
    }

    pub fn as_mut(self) -> &'tx mut T {
        unsafe { &mut *(marker::persistent_addr_mut(self.id) as *mut T) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Pair {
        a: u32,
        b: u32,
    }

    unsafe impl Persist for Pair {
        const TYPE_NAME: &'static str = "Pair";
    }

    #[test]
    fn root_keeps_typed_persistent_identity() {
        let root = Root::<Pair>::tmemory_offset(64);
        assert_eq!(root.id().payload(), 64);
    }
}
```

- [ ] **Step 4: Run SDK tests**

Run:

```bash
cargo test -p wasmtime-transaction-sdk -- --format terse
```

Expected: all SDK unit tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-sdk
git commit -m "Add Rust transaction SDK core types"
```

---

## Task 3: Implement SDK Macros And Metadata Emission

**Files:**
- Modify: `crates/transaction-sdk-macros/src/lib.rs`
- Modify: `crates/transaction-sdk/src/lib.rs`
- Test: `crates/transaction-sdk/tests/macros.rs`

- [ ] **Step 1: Write macro integration tests**

Create `crates/transaction-sdk/tests/macros.rs`:

```rust
use wasmtime_transaction_sdk::{Persist, Root};

#[derive(Persist)]
#[repr(C)]
struct Account {
    balance: i64,
}

#[derive(Persist)]
#[repr(C)]
struct Bank {
    accounts: [Account; 2],
}

#[test]
fn derive_persist_uses_rust_layout_values() {
    assert_eq!(Account::TYPE_NAME, "Account");
    assert_eq!(Account::SIZE, core::mem::size_of::<Account>());
    assert_eq!(Bank::ALIGN, core::mem::align_of::<Bank>());
}

#[test]
fn root_type_checks_derived_persist_types() {
    let root = Root::<Bank>::tmemory_offset(0);
    assert_eq!(root.id().payload(), 0);
}
```

- [ ] **Step 2: Run the failing macro tests**

Run:

```bash
cargo test -p wasmtime-transaction-sdk --test macros -- --format terse
```

Expected: FAIL because `#[derive(Persist)]` currently returns the input unchanged.

- [ ] **Step 3: Implement `#[derive(Persist)]`**

Replace `crates/transaction-sdk-macros/src/lib.rs` with:

```rust
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, FnArg, ItemFn, Type};

#[proc_macro_derive(Persist)]
pub fn derive_persist(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    let ident = input.ident;
    let type_name = ident.to_string();
    let type_name_len = type_name.len();
    let type_name_bytes = proc_macro2::Literal::byte_string(type_name.as_bytes());
    let metadata_ident = format_ident!("__TWASM_PERSIST_METADATA_{}", ident);

    quote! {
        unsafe impl ::wasmtime_transaction_sdk::Persist for #ident {
            const TYPE_NAME: &'static str = #type_name;
        }

        #[used]
        #[cfg_attr(target_arch = "wasm32", unsafe(link_section = "twasm.persist"))]
        static #metadata_ident: [u8; #type_name_len] = *#type_name_bytes;
    }
    .into()
}

#[proc_macro_attribute]
pub fn transaction(_args: TokenStream, input: TokenStream) -> TokenStream {
    let mut func = parse_macro_input!(input as ItemFn);
    let persistent_arg_markers = func
        .sig
        .inputs
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| match arg {
            FnArg::Typed(arg) => match &*arg.ty {
                Type::Reference(reference) if reference.mutability.is_some() => {
                    let index = index as u32;
                    Some(quote!(::wasmtime_transaction_sdk::marker::mark_persistent_arg(#index);))
                }
                _ => None,
            },
            FnArg::Receiver(_) => None,
        })
        .collect::<Vec<_>>();
    let block = func.block;
    func.block = Box::new(syn::parse_quote!({
        ::wasmtime_transaction_sdk::marker::mark_transaction_func();
        #(#persistent_arg_markers)*
        #block
    }));
    quote!(#func).into()
}
```

- [ ] **Step 4: Export the marker module**

Ensure `crates/transaction-sdk/src/lib.rs` keeps `pub mod marker;` public because `#[transaction]` emits calls to it.

- [ ] **Step 5: Run macro tests**

Run:

```bash
cargo test -p wasmtime-transaction-sdk --test macros -- --format terse
cargo test -p wasmtime-transaction-sdk-macros -- --format terse
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/transaction-sdk crates/transaction-sdk-macros
git commit -m "Add Rust transaction SDK macros"
```

---

## Task 4: Add Postpass CLI And Metadata Inspection

**Files:**
- Modify: `crates/transaction-tools/src/rewrite.rs`
- Create: `crates/transaction-tools/src/metadata.rs`
- Modify: `crates/transaction-tools/src/lib.rs`
- Modify: `crates/transaction-tools/src/main.rs`
- Test: `crates/transaction-tools/tests/inspect.rs`

- [ ] **Step 1: Write metadata inspection test**

Create `crates/transaction-tools/tests/inspect.rs`:

```rust
use wasm_encoder::{CustomSection, Module};
use wasmtime_transaction_tools::metadata::inspect_module;

#[test]
fn inspect_finds_persist_metadata_section() {
    let mut module = Module::new();
    module.section(&CustomSection {
        name: "twasm.persist".into(),
        data: b"Bank".as_slice().into(),
    });

    let report = inspect_module(&module.finish()).unwrap();
    assert_eq!(report.persist_sections, 1);
    assert_eq!(report.persist_payloads, vec!["Bank"]);
}
```

- [ ] **Step 2: Run the failing inspection test**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test inspect -- --format terse
```

Expected: FAIL because `metadata` is not implemented.

- [ ] **Step 3: Implement metadata inspection**

Create `crates/transaction-tools/src/metadata.rs`:

```rust
use anyhow::{Context, Result};
use serde::Serialize;
use wasmparser::{Parser, Payload};

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct MetadataReport {
    pub persist_sections: usize,
    pub persist_payloads: Vec<String>,
}

pub fn inspect_module(bytes: &[u8]) -> Result<MetadataReport> {
    let mut report = MetadataReport::default();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.context("failed to parse wasm payload")?;
        if let Payload::CustomSection(section) = payload {
            if section.name() == "twasm.persist" {
                report.persist_sections += 1;
                let text = core::str::from_utf8(section.data())
                    .context("twasm.persist section is not utf-8")?
                    .to_owned();
                report.persist_payloads.push(text);
            }
        }
    }

    Ok(report)
}
```

Modify `crates/transaction-tools/src/lib.rs`:

```rust
pub mod metadata;
pub mod rewrite;

pub use metadata::{MetadataReport, inspect_module};
pub use rewrite::{RewriteReport, rewrite_module};
```

Create `crates/transaction-tools/src/rewrite.rs`:

```rust
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct RewriteReport {
    pub transaction_functions: usize,
    pub persistent_addr_markers: usize,
    pub i32_tloads: usize,
    pub i64_tloads: usize,
    pub f32_tloads: usize,
    pub f64_tloads: usize,
    pub i32_tstores: usize,
    pub i64_tstores: usize,
    pub f32_tstores: usize,
    pub f64_tstores: usize,
}

pub fn rewrite_module(input: &[u8]) -> Result<(Vec<u8>, RewriteReport)> {
    Ok((input.to_vec(), RewriteReport::default()))
}

pub fn main() -> anyhow::Result<()> {
    crate::cli::main()
}
```

Create `crates/transaction-tools/src/cli.rs`:

```rust
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Inspect {
        input: PathBuf,
    },
    Rewrite {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { input } => {
            let bytes = fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let report = crate::metadata::inspect_module(&bytes)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Rewrite { input, output, report } => {
            let bytes = fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let (rewritten, rewrite_report) = crate::rewrite::rewrite_module(&bytes)?;
            fs::write(&output, rewritten).with_context(|| format!("failed to write {}", output.display()))?;
            if let Some(report_path) = report {
                fs::write(&report_path, serde_json::to_vec_pretty(&rewrite_report)?)
                    .with_context(|| format!("failed to write {}", report_path.display()))?;
            }
        }
    }
    Ok(())
}
```

Modify `crates/transaction-tools/src/lib.rs` to add `pub mod cli;`.

- [ ] **Step 4: Run tool checks**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test inspect -- --format terse
cargo check -p wasmtime-transaction-tools
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools
git commit -m "Add Rust transaction postpass inspection"
```

---

## Task 5: Lower Persistent TMemory Roots And Transactional Loads/Stores

**Files:**
- Modify: `crates/transaction-tools/src/rewrite.rs`
- Create: `crates/transaction-tools/tests/rewrite_tmemory.rs`

- [ ] **Step 1: Add rewrite tests over a minimal Wasm fixture**

Create `crates/transaction-tools/tests/rewrite_tmemory.rs`:

```rust
use wasmtime_transaction_tools::rewrite_module;

fn persistent_store_fixture() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (memory 1)
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut"
            (func $__twasm_persistent_addr_mut (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func"
            (func $__twasm_mark_transaction_func))
          (func $transfer (param $root i64) (param $delta i32)
            (local $ptr i32)
            call $__twasm_mark_transaction_func
            local.get $root
            call $__twasm_persistent_addr_mut
            local.tee $ptr
            local.get $ptr
            i32.load
            local.get $delta
            i32.add
            i32.store)
          (export "transfer" (func $transfer)))
        "#,
    )
    .unwrap()
}

#[test]
fn rewrite_replaces_markers_and_reports_transactional_accesses() {
    let input = persistent_store_fixture();
    let (output, report) = rewrite_module(&input).unwrap();
    let wat = wasmprinter::print_bytes(&output).unwrap();

    assert_eq!(report.transaction_functions, 1);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert_eq!(report.i32_tstores, 1);
    assert!(!wat.contains("__twasm_persistent_addr_mut"));
    assert!(!wat.contains("__twasm_mark_transaction_func"));
    assert!(wat.contains("i32.tload") || wat.contains("i32.load"));
    assert!(wat.contains("i32.tstore") || wat.contains("i32.store"));
}
```

The final two assertions allow the test to compile before wasm-printer spelling is confirmed. Tighten them to require the transaction opcode spelling once the local wasm-tools fork prints transactional memory ops consistently.

- [ ] **Step 2: Run the failing rewrite test**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test rewrite_tmemory -- --format terse
```

Expected: FAIL because the rewrite currently returns the input unchanged and all counts are zero.

- [ ] **Step 3: Implement the first rewrite pass**

Implement `rewrite_module` with these concrete rules:

- Parse the module with `wasmparser`.
- Identify imported functions:
  - module `twasm_intrinsics`, field `__twasm_persistent_addr_mut`
  - module `twasm_intrinsics`, field `__twasm_mark_transaction_func`
  - module `twasm_intrinsics`, field `__twasm_mark_persistent_arg`
- Rewrite each function body:
  - remove `call __twasm_mark_transaction_func` and count the containing function as transactional,
  - remove `i32.const <n>; call __twasm_mark_persistent_arg` and mark parameter `<n>` as a persistent taint source,
  - replace `call __twasm_persistent_addr_mut` with:
    - `i64.const 0x0fff_ffff_ffff_ffff`
    - `i64.and`
    - `i32.wrap_i64`
  - track locals receiving that value through `local.tee`, `local.set`, `local.get`, and `i32.add` with constants,
  - rewrite `i32.load`, `i64.load`, `f32.load`, `f64.load`, `i32.store`, `i64.store`, `f32.store`, and `f64.store` using tainted address expressions to the matching transactional memory opcode,
  - allow calls from one transaction-marked function into another transaction-marked function when tainted pointer arguments flow into parameters marked by `__twasm_mark_persistent_arg`.
- Rewrite the default linear memory declaration/import used by persistent roots into a transactional memory declaration/import so the generated `*.tload`/`*.tstore` operators execute through the same `tmemory` runtime path as the simple-transactions WAST suite.
- Reject unsupported pointer escapes with an `anyhow::bail!` message:
  - `persistent pointer escapes to unrecognized call in function index {index}`

Keep the first implementation limited to scalar numeric loads/stores. Packed integer loads/stores, SIMD, and object ops are separate follow-up tasks once the bank and pressure fixtures prove the full path.

- [ ] **Step 4: Run rewrite tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test rewrite_tmemory -- --format terse
cargo test -p wasmtime-transaction-tools -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools
git commit -m "Lower Rust persistent tmemory markers"
```

---

## Task 6: Add `twasm-rust build` Wrapper

**Files:**
- Modify: `crates/transaction-tools/src/cli.rs`
- Test: `crates/transaction-tools/tests/build_cli.rs`

- [ ] **Step 1: Add CLI argument parsing test**

Create `crates/transaction-tools/tests/build_cli.rs`:

```rust
use clap::Parser;
use wasmtime_transaction_tools::cli::Cli;

#[test]
fn build_subcommand_accepts_package_and_output() {
    Cli::parse_from([
        "twasm-rust",
        "build",
        "--manifest-path",
        "examples/transaction-rust/bank/Cargo.toml",
        "--output",
        "target/transaction-rust/bank.twasm.wasm",
    ]);
}
```

- [ ] **Step 2: Run the failing CLI test**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test build_cli -- --format terse
```

Expected: FAIL because the `Build` subcommand is not defined.

- [ ] **Step 3: Implement the build wrapper**

Add this command variant to `crates/transaction-tools/src/cli.rs`:

```rust
Build {
    #[arg(long)]
    manifest_path: PathBuf,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long)]
    release: bool,
    #[arg(long)]
    report: Option<PathBuf>,
}
```

Add handling with this behavior:

- Run:

```bash
cargo build --target wasm32-unknown-unknown --manifest-path <manifest-path>
```

- Add `--release` when requested.
- Derive the input wasm path from the manifest package name:
  - debug: `target/wasm32-unknown-unknown/debug/<package_name>.wasm`
  - release: `target/wasm32-unknown-unknown/release/<package_name>.wasm`
- Run `rewrite_module` on that file.
- Write the rewritten module to `--output`.
- Write JSON `RewriteReport` when `--report` is provided.

- [ ] **Step 4: Run CLI tests**

Run:

```bash
cargo test -p wasmtime-transaction-tools --test build_cli -- --format terse
cargo check -p wasmtime-transaction-tools
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/transaction-tools
git commit -m "Add Rust transaction build wrapper"
```

---

## Task 7: Add Reusable Rust Guest Fixtures

**Files:**
- Create: `examples/transaction-rust/bank/Cargo.toml`
- Create: `examples/transaction-rust/bank/src/lib.rs`
- Create: `examples/transaction-rust/pressure/Cargo.toml`
- Create: `examples/transaction-rust/pressure/src/lib.rs`

- [ ] **Step 1: Add the bank demo guest**

Create `examples/transaction-rust/bank/Cargo.toml`:

```toml
[package]
name = "transaction-rust-bank"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
wasmtime-transaction-sdk = { path = "../../../crates/transaction-sdk" }
```

Create `examples/transaction-rust/bank/src/lib.rs`:

```rust
#![no_std]

use wasmtime_transaction_sdk::{Persist, Root, transaction};

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Account {
    pub balance: i64,
}

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Bank {
    pub accounts: [Account; 2],
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Request {
    pub from: u32,
    pub to: u32,
    pub amount: i64,
}

const BANK_ROOT: Root<Bank> = Root::tmemory_offset(0);

#[unsafe(no_mangle)]
pub extern "C" fn init_bank(a: i64, b: i64) {
    wasmtime_transaction_sdk::transaction!(tx, {
        let mut bank = tx.root_mut(&BANK_ROOT);
        let bank = bank.as_mut();
        bank.accounts[0].balance = a;
        bank.accounts[1].balance = b;
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn transfer(from: u32, to: u32, amount: i64) {
    let req = Request { from, to, amount };
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_mut(&BANK_ROOT);
        transfer_impl(bank.as_mut(), &req);
    })
}

#[transaction]
fn transfer_impl(bank: &mut Bank, req: &Request) {
    bank.accounts[req.from as usize].balance -= req.amount;
    bank.accounts[req.to as usize].balance += req.amount;
}

#[unsafe(no_mangle)]
pub extern "C" fn balance(index: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        let bank = tx.root_ref(&BANK_ROOT);
        bank.as_ref().accounts[index as usize].balance
    })
}
```

- [ ] **Step 2: Add a pressure guest**

Create `examples/transaction-rust/pressure/Cargo.toml`:

```toml
[package]
name = "transaction-rust-pressure"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
wasmtime-transaction-sdk = { path = "../../../crates/transaction-sdk" }
```

Create `examples/transaction-rust/pressure/src/lib.rs`:

```rust
#![no_std]

use wasmtime_transaction_sdk::{Persist, Root, transaction};

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Cell {
    pub value: i64,
}

#[derive(Clone, Copy, Persist)]
#[repr(C)]
pub struct Region {
    pub cells: [Cell; 64],
    pub i32_slot: i32,
    pub i64_slot: i64,
    pub f32_slot: f32,
    pub f64_slot: f64,
}

const REGION_ROOT: Root<Region> = Root::tmemory_offset(4096);

#[transaction]
#[unsafe(no_mangle)]
pub extern "C" fn pressure_round(iterations: u32, stride: u32) -> i64 {
    wasmtime_transaction_sdk::transaction!(tx, {
        let region = tx.root_mut(&REGION_ROOT);
        let region = region.as_mut();
        let mut checksum = 0i64;
        let mut i = 0;
        while i < iterations {
            let index = ((i * stride) & 63) as usize;
            region.cells[index].value += i as i64;
            checksum += region.cells[index].value;
            i += 1;
        }
        region.i32_slot += iterations as i32;
        region.i64_slot += checksum;
        region.f32_slot += iterations as f32;
        region.f64_slot += checksum as f64;
        checksum += region.i32_slot as i64;
        checksum += region.i64_slot;
        checksum += region.f32_slot as i64;
        checksum += region.f64_slot as i64;
        checksum
    })
}
```

- [ ] **Step 3: Build examples through the wrapper**

Run:

```bash
cargo run -p wasmtime-transaction-tools --bin twasm-rust -- build \
  --manifest-path examples/transaction-rust/bank/Cargo.toml \
  --output target/transaction-rust/bank.twasm.wasm \
  --report target/transaction-rust/bank.report.json

cargo run -p wasmtime-transaction-tools --bin twasm-rust -- build \
  --manifest-path examples/transaction-rust/pressure/Cargo.toml \
  --output target/transaction-rust/pressure.twasm.wasm \
  --report target/transaction-rust/pressure.report.json
```

Expected: both commands produce rewritten Wasm and JSON reports with nonzero transactional load/store counts. If `wasm32-unknown-unknown` is missing, install it with `rustup target add wasm32-unknown-unknown` and rerun.

- [ ] **Step 4: Commit**

```bash
git add examples/transaction-rust
git commit -m "Add Rust transaction demo fixtures"
```

---

## Task 8: Add End-To-End File-Backed Recovery Test For Rust Guests

**Files:**
- Create: `tests/transaction_rust_toolset.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Write host integration test**

Create `tests/transaction_rust_toolset.rs`:

```rust
#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use wasmtime::{Config, Engine, Func, Instance, Module, Store};

fn rust_target_available() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|stdout| stdout.lines().any(|line| line == "wasm32-unknown-unknown"))
}

fn build_guest(manifest: &str, output: &Path) -> Result<()> {
    if !rust_target_available() {
        bail!("wasm32-unknown-unknown target is not installed");
    }

    let status = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-rust",
            "--",
            "build",
            "--manifest-path",
            manifest,
            "--output",
        ])
        .arg(output)
        .status()
        .context("failed to run twasm-rust build")?;

    if !status.success() {
        bail!("twasm-rust build failed with {status}");
    }

    Ok(())
}

fn instantiate(path: &Path, tmemory_path: &Path, log_path: &Path, create: bool) -> Result<(Store<()>, Instance)> {
    let mut config = Config::new();
    config.wasm_threads(true);
    config.wasm_gc(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, path)?;
    let mut store = Store::new(&engine, ());
    if create {
        store.transaction_create_file_backed_storage_for_test(
            tmemory_path.to_path_buf(),
            log_path.to_path_buf(),
        )?;
    } else {
        store.transaction_open_file_backed_storage_for_test(
            tmemory_path.to_path_buf(),
            log_path.to_path_buf(),
        )?;
    }
    let instance = Instance::new(&mut store, &module, &[])?;
    Ok((store, instance))
}

fn get_func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance.get_func(&mut *store, name).with_context(|| format!("missing export {name}"))
}

#[test]
fn rust_bank_guest_survives_file_backed_recovery() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("bank.twasm.wasm");
    let tmemory_path = temp.path().join("bank.tmemory");
    let log_path = temp.path().join("bank.txlog");

    build_guest("examples/transaction-rust/bank/Cargo.toml", &wasm_path)?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &log_path, true)?;
        get_func(&mut store, &instance, "init_bank")?
            .typed::<(i64, i64), ()>(&store)?
            .call(&mut store, (100, 50))?;
        get_func(&mut store, &instance, "transfer")?
            .typed::<(u32, u32, i64), ()>(&store)?
            .call(&mut store, (0, 1, 7))?;
    }

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &log_path,
        &tmemory_path,
    )?;

    {
        let (mut store, instance) = instantiate(&wasm_path, &tmemory_path, &log_path, false)?;
        let balance = get_func(&mut store, &instance, "balance")?.typed::<u32, i64>(&store)?;
        assert_eq!(balance.call(&mut store, 0)?, 93);
        assert_eq!(balance.call(&mut store, 1)?, 57);
    }

    Ok(())
}
```

- [ ] **Step 2: Run the end-to-end test**

Run:

```bash
cargo test --test transaction_rust_toolset rust_bank_guest_survives_file_backed_recovery -- --format terse
```

Expected: PASS when `wasm32-unknown-unknown` is installed. If the target is missing, the test fails with `wasm32-unknown-unknown target is not installed`; install the target and rerun.

- [ ] **Step 3: Document the Rust frontend status**

Append to `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
## Rust Transaction Toolset

- Added a Rust SDK frontend for typed persistent roots and transaction-scoped references.
- Added `twasm-rust`, a Rust/Wasm postpass wrapper that lowers SDK marker intrinsics into transactional Wasm.
- The first supported backend is typed roots over file-backed `tmemory`; the persistent ID encoding reserves object IDs for the persistent-GC/object-storage path.
- The bank fixture is only the first example. Pressure tests should add new Rust guest crates under `examples/transaction-rust/` and compile them through `twasm-rust`.
```

- [ ] **Step 4: Commit**

```bash
git add tests/transaction_rust_toolset.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add Rust transaction toolset recovery test"
```

---

## Task 9: Verify Rust Output Against Simple-Transactions Shape

**Files:**
- Create: `tests/transaction_rust_simple_transactions.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add a postpass-output oracle test**

Create `tests/transaction_rust_simple_transactions.rs`:

```rust
#![cfg(all(feature = "transaction", unix))]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use wasmtime::{Config, Engine, Func, Instance, Module, Store};
use wasmparser::{Operator, Parser, Payload};

fn rust_target_available() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|stdout| stdout.lines().any(|line| line == "wasm32-unknown-unknown"))
}

fn build_guest(manifest: &str, output: &Path) -> Result<()> {
    if !rust_target_available() {
        bail!("wasm32-unknown-unknown target is not installed");
    }

    let status = Command::new(env!("CARGO"))
        .args([
            "run",
            "-p",
            "wasmtime-transaction-tools",
            "--bin",
            "twasm-rust",
            "--",
            "build",
            "--manifest-path",
            manifest,
            "--output",
        ])
        .arg(output)
        .status()
        .context("failed to run twasm-rust build")?;

    if !status.success() {
        bail!("twasm-rust build failed with {status}");
    }

    Ok(())
}

#[derive(Default)]
struct TransactionShape {
    i32_tloads: usize,
    i64_tloads: usize,
    f32_tloads: usize,
    f64_tloads: usize,
    i32_tstores: usize,
    i64_tstores: usize,
    f32_tstores: usize,
    f64_tstores: usize,
    marker_imports: usize,
}

fn inspect_transaction_shape(bytes: &[u8]) -> Result<TransactionShape> {
    let mut shape = TransactionShape::default();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.context("failed to parse rewritten Rust guest")? {
            Payload::ImportSection(imports) => {
                for import in imports {
                    let import = import.context("failed to parse import")?;
                    if import.module == "twasm_intrinsics" {
                        shape.marker_imports += 1;
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut reader = body.get_operators_reader()?;
                while !reader.eof() {
                    match reader.read()? {
                        Operator::I32TLoad { .. } => shape.i32_tloads += 1,
                        Operator::I64TLoad { .. } => shape.i64_tloads += 1,
                        Operator::F32TLoad { .. } => shape.f32_tloads += 1,
                        Operator::F64TLoad { .. } => shape.f64_tloads += 1,
                        Operator::I32TStore { .. } => shape.i32_tstores += 1,
                        Operator::I64TStore { .. } => shape.i64_tstores += 1,
                        Operator::F32TStore { .. } => shape.f32_tstores += 1,
                        Operator::F64TStore { .. } => shape.f64_tstores += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(shape)
}

fn instantiate_with_wasmtime_variant(path: &Path) -> Result<(Store<()>, Instance)> {
    let mut config = Config::new();
    config.wasm_threads(true);
    config.wasm_gc(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, path)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;
    Ok((store, instance))
}

fn get_func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance.get_func(&mut *store, name).with_context(|| format!("missing export {name}"))
}

#[test]
fn rust_pressure_postpass_outputs_all_scalar_transaction_ops() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("pressure.twasm.wasm");
    build_guest("examples/transaction-rust/pressure/Cargo.toml", &wasm_path)?;

    let bytes = std::fs::read(&wasm_path)?;
    let wat = wasmprinter::print_bytes(&bytes)?;
    let shape = inspect_transaction_shape(&bytes)?;

    assert_eq!(shape.marker_imports, 0, "SDK marker imports must be removed by the postpass");
    assert!(shape.i32_tloads > 0, "rewritten Rust guest should contain i32.tload");
    assert!(shape.i64_tloads > 0, "rewritten Rust guest should contain i64.tload");
    assert!(shape.f32_tloads > 0, "rewritten Rust guest should contain f32.tload");
    assert!(shape.f64_tloads > 0, "rewritten Rust guest should contain f64.tload");
    assert!(shape.i32_tstores > 0, "rewritten Rust guest should contain i32.tstore");
    assert!(shape.i64_tstores > 0, "rewritten Rust guest should contain i64.tstore");
    assert!(shape.f32_tstores > 0, "rewritten Rust guest should contain f32.tstore");
    assert!(shape.f64_tstores > 0, "rewritten Rust guest should contain f64.tstore");
    assert!(wat.contains("i32.tload"), "printed WAT should expose i32.tload syntax");
    assert!(wat.contains("i64.tload"), "printed WAT should expose i64.tload syntax");
    assert!(wat.contains("f32.tload"), "printed WAT should expose f32.tload syntax");
    assert!(wat.contains("f64.tload"), "printed WAT should expose f64.tload syntax");
    assert!(wat.contains("i32.tstore"), "printed WAT should expose i32.tstore syntax");
    assert!(wat.contains("i64.tstore"), "printed WAT should expose i64.tstore syntax");
    assert!(wat.contains("f32.tstore"), "printed WAT should expose f32.tstore syntax");
    assert!(wat.contains("f64.tstore"), "printed WAT should expose f64.tstore syntax");
    assert!(!wat.contains("twasm_intrinsics"), "printed WAT must not retain SDK marker ABI");

    Ok(())
}

#[test]
fn rust_bank_postpass_runs_on_wasmtime_transaction_variant() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let wasm_path: PathBuf = temp.path().join("bank.twasm.wasm");
    build_guest("examples/transaction-rust/bank/Cargo.toml", &wasm_path)?;

    let (mut store, instance) = instantiate_with_wasmtime_variant(&wasm_path)?;

    get_func(&mut store, &instance, "init_bank")?
        .typed::<(i64, i64), ()>(&store)?
        .call(&mut store, (100, 50))?;
    get_func(&mut store, &instance, "transfer")?
        .typed::<(u32, u32, i64), ()>(&store)?
        .call(&mut store, (0, 1, 7))?;

    let balance = get_func(&mut store, &instance, "balance")?.typed::<u32, i64>(&store)?;
    assert_eq!(balance.call(&mut store, 0)?, 93);
    assert_eq!(balance.call(&mut store, 1)?, 57);

    Ok(())
}
```

- [ ] **Step 2: Run the failing oracle test**

Run:

```bash
cargo test --test transaction_rust_simple_transactions rust_pressure_postpass_outputs_all_scalar_transaction_ops -- --format terse
cargo test --test transaction_rust_simple_transactions rust_bank_postpass_runs_on_wasmtime_transaction_variant -- --format terse
```

Expected before Task 5 is complete: the shape test fails because the postpass still returns the input module unchanged, and the Wasmtime execution test fails because SDK marker imports remain unresolved. Expected after Task 5 and Task 7 are complete: both pass, with nonzero scalar transactional load/store counts for `i32`, `i64`, `f32`, and `f64`, plus successful execution on this Wasmtime transaction variant.

- [ ] **Step 3: Add the simple-transactions WAST compatibility gate**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tload.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstore.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory.wast -- --format terse
```

Expected: all three pass. This confirms the instruction family used by the Rust postpass is still accepted by the same parser/runtime path as the simple-transactions proposal fixtures.

- [ ] **Step 4: Document the oracle boundary**

Append to `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
## Rust Frontend Simple-Transactions Oracle

- The Rust SDK marker ABI is frontend-only. It must not appear in rewritten modules.
- `twasm-rust` output is checked for real simple-transactions scalar operators: `i32.tload`, `i64.tload`, `f32.tload`, `f64.tload`, and matching stores.
- The rewritten Rust guest is instantiated and executed by this Wasmtime transaction branch, proving the postpass output is not only printable but accepted by the active runtime variant.
- The simple-transactions WAST fixtures remain the semantic oracle for the postpass output boundary; the OCaml prototype can be used there because it sees rewritten transactional Wasm, not SDK markers.
```

- [ ] **Step 5: Commit**

```bash
git add tests/transaction_rust_simple_transactions.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Verify Rust postpass against simple transactions"
```

---

## Task 10: Final Verification

**Files:**
- No source edits unless a verification failure identifies a bug.

- [ ] **Step 1: Run SDK and tool tests**

Run:

```bash
cargo test -p wasmtime-transaction-sdk -- --format terse
cargo test -p wasmtime-transaction-sdk-macros -- --format terse
cargo test -p wasmtime-transaction-tools -- --format terse
```

Expected: all pass.

- [ ] **Step 2: Run transaction runtime regression tests**

Run:

```bash
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test --test transaction_persistence_wast -- --format terse
```

Expected: all pass with the same ignored-test count as before this plan.

- [ ] **Step 3: Run transaction WAST suite**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: all transaction WAST cases pass with no ignored cases.

- [ ] **Step 4: Run Rust guest end-to-end test**

Run:

```bash
cargo test --test transaction_rust_toolset -- --format terse
cargo test --test transaction_rust_simple_transactions -- --format terse
```

Expected: all tests pass when the Rust `wasm32-unknown-unknown` target is installed.

- [ ] **Step 5: Run formatting and diff checks**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: both pass.

- [ ] **Step 6: Commit verification fixes if needed**

If a verification command required a source fix, commit only the fix files:

```bash
git add <fixed-files>
git commit -m "Fix Rust transaction toolset verification"
```

---

## Scope Boundaries

- This plan does not modify `rustc`.
- This plan does not require a persistent GC.
- This plan does not implement transactional persistent `Vec`, `String`, trait objects, or object references.
- This plan does not make SDK calls intercept every load/store at runtime.
- This plan intentionally uses ordinary Rust syntax inside marked code and relies on the postpass to turn persistent-provenance accesses into transactional Wasm.

## Self-Review

- Spec coverage: the plan covers SDK runtime role, annotations, metadata, postpass lowering, reusable examples, pressure-test entry points, simple-transactions postpass-output verification, and end-to-end file-backed recovery.
- Placeholder scan: all tasks have concrete file paths, code, commands, and expected outcomes.
- Type consistency: `PersistentId`, `Root<T>`, `Tx<'tx>`, `PRef<'tx, T>`, `PMut<'tx, T>`, `Persist`, `rewrite_module`, and `RewriteReport` are named consistently across tasks.
