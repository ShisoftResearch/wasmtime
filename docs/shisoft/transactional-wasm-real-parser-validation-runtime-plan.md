# Transactional Wasm Parser Validation Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace research-only normalization/stubs with real text syntax,
transactional object validation, and minimal `VMemory` runtime semantics.

**Architecture:** Work proceeds in phase order. First, the local
`wasm-tools-transaction` fork learns real milestone-1 text instruction syntax
and emits the same `0xfa` binary encodings already accepted by Wasmtime. Next,
Wasmtime records enough transaction object metadata to validate ordinary versus
transactional object spaces. Finally, runtime helpers stop trapping for the
minimal `VMemory` backend and use copy-on-write transaction staging.

**Tech Stack:** Rust, local wasm-tools fork (`wast`, `wat`, `wasmparser`,
`wasm-encoder`), Wasmtime `environ`, Cranelift lowering, Wasmtime runtime
libcalls, proposal WAST harness.

---

## Scope

This plan intentionally implements milestone-1 scalar behavior first.

In scope:

- Real WAST/WAT text syntax for milestone-1 transaction operators.
- Validation metadata for transactional memories and globals.
- Minimal runtime semantics for `VMemory`, numeric transactional globals, and
  scalar/packed integer transactional memory operations.
- Proposal WAST migration from text normalization to real parser execution for
  the first safe files.

Out of scope for this plan:

- `FileBackedMemory` and `NVMemory`.
- Transactional tables, refs, GC objects, and durability.
- SIMD transaction operators except parser inventory notes and explicit
  unsupported tests.
- Full upstream-quality feature configuration; this remains research-fork
  gated.

## Files

- Modify:
  `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/expr.rs`
  for text instruction variants and binary encodings.
- Add tests near existing wasm-tools parser/operator tests in
  `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/expr.rs`
  or the crate's existing test module if more local.
- Modify:
  `crates/test-util/src/wast.rs` and `tests/wast.rs` to allow selected
  proposal files to parse without transaction-text normalization.
- Modify:
  `crates/environ/src/module.rs`,
  `crates/environ/src/compile/module_environ.rs`, and
  `crates/environ/src/transaction.rs` for transactional object metadata and
  validation helpers.
- Modify:
  `crates/wasmtime/src/runtime/transaction.rs` for minimal `VMemory`, staged
  COW helpers, and tests.
- Modify:
  `crates/wasmtime/src/runtime/vm/libcalls.rs` to route helper ABI calls into
  runtime transaction state.
- Update:
  `docs/shisoft/transactional-wasm-action-plan.md` and
  `docs/shisoft/transactional-wasm-implementation-log.md` after each phase.

---

### Task 1: Add Real Milestone-1 Text Instruction Syntax

**Files:**

- Modify:
  `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wast/src/core/expr.rs`
- Verify from Wasmtime:
  `cargo test -p wasmtime --lib module_compilation_accepts_lifecycle_transaction_opcodes`
- Verify from wasm-tools fork:
  `cargo test -p wast transaction_text`

- [ ] **Step 1: Write failing WAST text parser tests**

Add a test module in `crates/wast/src/core/expr.rs` with fixtures that parse
and encode the following text snippets through `wat::parse_str`:

```wat
(module (func (ttry) (tfail)))
```

```wat
(module
  (global (mut i32) (i32.const 0))
  (func (result i32) (tglobal.get 0))
  (func (param i32) (local.get 0) (tglobal.set 0)))
```

```wat
(module
  (memory 1)
  (func (result i32) (i32.const 0) (i32.tload))
  (func (i32.const 0) (i32.const 1) (i32.tstore))
  (func (result i32) (tmemory.size))
  (func (result i32) (i32.const 1) (tmemory.grow)))
```

Expected failure before implementation: unknown operator or unexpected token
for `ttry`, `tglobal.get`, or `i32.tload`.

- [ ] **Step 2: Extend `Instruction<'a>` table**

Add variants in the macro table using the already-established binary
encodings:

```rust
TTry : [0xfa, 0x04] : "ttry",
TFail : [0xfa, 0x0f] : "tfail",
TGlobalGet(Index<'a>) : [0xfa, 0x23] : "tglobal.get",
TGlobalSet(Index<'a>) : [0xfa, 0x24] : "tglobal.set",
I32TLoad(MemArg<4>) : [0xfa, 0x28] : "i32.tload",
I64TLoad(MemArg<8>) : [0xfa, 0x29] : "i64.tload",
F32TLoad(MemArg<4>) : [0xfa, 0x2a] : "f32.tload",
F64TLoad(MemArg<8>) : [0xfa, 0x2b] : "f64.tload",
I32TLoad8s(MemArg<1>) : [0xfa, 0x2c] : "i32.tload8_s",
I32TLoad8u(MemArg<1>) : [0xfa, 0x2d] : "i32.tload8_u",
I32TLoad16s(MemArg<2>) : [0xfa, 0x2e] : "i32.tload16_s",
I32TLoad16u(MemArg<2>) : [0xfa, 0x2f] : "i32.tload16_u",
I64TLoad8s(MemArg<1>) : [0xfa, 0x30] : "i64.tload8_s",
I64TLoad8u(MemArg<1>) : [0xfa, 0x31] : "i64.tload8_u",
I64TLoad16s(MemArg<2>) : [0xfa, 0x32] : "i64.tload16_s",
I64TLoad16u(MemArg<2>) : [0xfa, 0x33] : "i64.tload16_u",
I64TLoad32s(MemArg<4>) : [0xfa, 0x34] : "i64.tload32_s",
I64TLoad32u(MemArg<4>) : [0xfa, 0x35] : "i64.tload32_u",
I32TStore(MemArg<4>) : [0xfa, 0x36] : "i32.tstore",
I64TStore(MemArg<8>) : [0xfa, 0x37] : "i64.tstore",
F32TStore(MemArg<4>) : [0xfa, 0x38] : "f32.tstore",
F64TStore(MemArg<8>) : [0xfa, 0x39] : "f64.tstore",
I32TStore8(MemArg<1>) : [0xfa, 0x3a] : "i32.tstore8",
I32TStore16(MemArg<2>) : [0xfa, 0x3b] : "i32.tstore16",
I64TStore8(MemArg<1>) : [0xfa, 0x3c] : "i64.tstore8",
I64TStore16(MemArg<2>) : [0xfa, 0x3d] : "i64.tstore16",
I64TStore32(MemArg<4>) : [0xfa, 0x3e] : "i64.tstore32",
TMemorySize(MemoryArg<'a>) : [0xfa, 0x3f] : "tmemory.size",
TMemoryGrow(MemoryArg<'a>) : [0xfa, 0x40] : "tmemory.grow",
```

- [ ] **Step 3: Run wasm-tools text tests**

Run:

```bash
cargo test -p wast transaction_text
```

Expected: all new transaction text parser tests pass.

- [ ] **Step 4: Run Wasmtime compile smoke tests**

Run from Wasmtime:

```bash
cargo test -p wasmtime --lib transaction
```

Expected: existing binary parser/lowering tests still pass.

- [ ] **Step 5: Commit**

Commit in the wasm-tools fork:

```bash
git -C /home/shisoft/Code/Research/wasm-tools-transaction add crates/wast/src/core/expr.rs
git -C /home/shisoft/Code/Research/wasm-tools-transaction commit -m "Add transaction text instruction syntax"
```

### Task 2: Move First WAST Files Off Normalization

**Files:**

- Modify: `crates/test-util/src/wast.rs`
- Modify: `tests/wast.rs`
- Test: selected proposal files under `../wasm-persistence/test/core`

- [ ] **Step 1: Add failing harness expectation**

Pick the first text-only files that use milestone-1 syntax but do not require
runtime execution beyond compilation diagnostics. Start with generated mini
fixtures if full proposal files execute too much runtime behavior.

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tload.wast -- --format terse
```

Expected before harness change: it still passes only through normalization or
remains ignored.

- [ ] **Step 2: Add explicit real-parser allowlist**

Add a small allowlist in `crates/test-util/src/wast.rs` or `tests/wast.rs`:

```rust
const TRANSACTION_REAL_TEXT_PARSER_FILES: &[&str] = &[
    "simple-transactions/tload.wast",
    "simple-transactions/tstore.wast",
    "simple-transactions/tmemory_size.wast",
    "simple-transactions/tmemory_grow.wast",
];
```

Files on this list must skip text normalization and use the real forked
`wast` parser.

- [ ] **Step 3: Keep runtime-heavy files ignored**

If a file compiles but fails because helper libcalls still trap, leave it
ignored with a blocker label in the ledger rather than reintroducing
normalization.

- [ ] **Step 4: Verify harness behavior**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo test -p wasmtime-test-util --features wast normalizes_transaction_proposal_text
```

Expected: existing normalized tranche remains stable, and real-parser files are
not rewritten.

- [ ] **Step 5: Commit**

```bash
git add crates/test-util/src/wast.rs tests/wast.rs docs/shisoft/transactional-wasm-wast-ledger.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Enable real parser path for transaction wast files"
```

### Task 3: Add Transactional Object Metadata Validation

**Files:**

- Modify: `crates/environ/src/module.rs`
- Modify: `crates/environ/src/compile/module_environ.rs`
- Modify: `crates/environ/src/transaction.rs`
- Test: `crates/environ/src/transaction.rs`

- [ ] **Step 1: Add failing validation tests**

Add tests that construct research metadata for:

- ordinary memory accessed by `i32.tload` must fail
- transactional memory accessed by ordinary `i32.load` must fail
- immutable transactional global accessed by `tglobal.set` must fail
- ordinary global accessed by `tglobal.get` must fail

Expected before implementation: at least ordinary-versus-transactional
distinction cannot be represented.

- [ ] **Step 2: Add module metadata containers**

Represent transaction object spaces without changing ordinary memory layout:

```rust
pub struct TransactionObjectSpaces {
    pub memories: PrimaryMap<MemoryIndex, TransactionMemoryKind>,
    pub globals: PrimaryMap<GlobalIndex, TransactionGlobalKind>,
}
```

For milestone 1, accepted kinds are only:

```rust
pub enum TransactionMemoryKind {
    VMemory,
}

pub enum TransactionGlobalKind {
    Numeric,
}
```

- [ ] **Step 3: Wire validation helper**

Add helper functions in `crates/environ/src/transaction.rs` that check an
operator against object metadata and return `WasmError::InvalidWebAssembly`
with messages containing `transactional memory` or `transactional global`.

- [ ] **Step 4: Run environ tests**

```bash
cargo test -p wasmtime-environ --lib transaction_
```

Expected: tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/environ/src/module.rs crates/environ/src/compile/module_environ.rs crates/environ/src/transaction.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transaction object validation metadata"
```

### Task 4: Implement Minimal `VMemory` COW Runtime Semantics

**Files:**

- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add failing runtime unit tests**

Add tests for:

- `tstore` stages bytes and does not mutate committed `VMemory` before commit.
- `tload` reads staged bytes first.
- crossing a `TMEMORY_GRANULE_SIZE` boundary stages both granules.
- abort/fail drops staged bytes.
- commit writes staged bytes into committed `VMemory`.
- `tmemory.grow` stages visible pages and commit applies them.

- [ ] **Step 2: Add `VMemory` storage**

Add a minimal volatile storage type:

```rust
pub(crate) struct VMemory {
    bytes: Vec<u8>,
    visible_pages: u64,
    page_size: u64,
}
```

Implement bounds-checked `read`, `write`, `grow`, and `size`.

- [ ] **Step 3: Extend transaction staged memory APIs**

Add helpers to `TransactionState`:

- `stage_tmemory_store`
- `read_tmemory`
- `stage_tmemory_grow`
- `commit_into_vmemory`

Use the shared `TMEMORY_GRANULE_SHIFT`/`TMEMORY_GRANULE_SIZE` constants.

- [ ] **Step 4: Wire libcall helpers**

Change helper stubs in `crates/wasmtime/src/runtime/vm/libcalls.rs` to call the
runtime transaction APIs. Keep unresolved real module object access as an
explicit error until task 3 metadata is wired into instances.

- [ ] **Step 5: Run runtime tests**

```bash
cargo test -p wasmtime --lib transaction
cargo check -p wasmtime
```

Expected: runtime unit tests pass and package check passes.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Implement minimal vmemory transaction staging"
```

### Task 5: SIMD Inventory And Explicit Deferral

**Files:**

- Modify:
  `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/operators.rs`
- Modify: `docs/shisoft/transactional-wasm-action-plan.md`
- Modify: `docs/shisoft/transactional-wasm-full-wast-roadmap.md`

- [ ] **Step 1: Inventory proposal SIMD operators**

List all `tsimd` transaction operators from `../wasm-persistence/test/core/tsimd`
and compare them to current parser support.

- [ ] **Step 2: Add explicit unsupported parser tests**

For out-of-scope SIMD transaction subopcodes, add tests that confirm they are
rejected as outside milestone 1 with clear diagnostics.

- [ ] **Step 3: Update roadmap**

Record SIMD as a post-scalar milestone unless runtime WAST migration proves a
specific SIMD operator is required earlier.

- [ ] **Step 4: Commit**

```bash
git add docs/shisoft/transactional-wasm-action-plan.md docs/shisoft/transactional-wasm-full-wast-roadmap.md
git -C /home/shisoft/Code/Research/wasm-tools-transaction add crates/wasmparser/src/readers/core/operators.rs
git commit -m "Document deferred transaction simd scope"
git -C /home/shisoft/Code/Research/wasm-tools-transaction commit -m "Reject out-of-scope transaction simd opcodes"
```

## Final Verification

Run from Wasmtime:

```bash
cargo test -p wasmtime-environ --lib transaction_
cargo test -p wasmtime --lib transaction
cargo check -p wasmtime
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Run from wasm-tools fork:

```bash
cargo test -p wast transaction_text
cargo test -p wasmparser transaction
```
