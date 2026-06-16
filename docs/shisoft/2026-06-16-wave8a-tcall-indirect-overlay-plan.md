# Wave 8A Tcall Indirect Overlay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make indirect calls through transactional tables observe the staged table overlay inside the active transaction.

**Architecture:** `ttable.set` remains private until commit. `tcall_indirect` and transactional return-call indirect must load the `VMFuncRef` pointer through the existing `transaction_ttable_get` builtin when the table is marked as a `ttable`; ordinary tables continue using the existing committed-table lowering.

**Tech Stack:** Wasmtime Cranelift lowering, Wasmtime transaction table runtime helpers, transaction proposal WAST tests.

---

### Task 1: Establish the Failing Transactional Indirect-Call Regression

**Files:**
- Reference only: `../wasm-persistence/test/core/simple-transactions/tref_tfunc.wast`
- Reference only: `../wasm-persistence/test/core/simple-transactions/tarray_init_elem.wast`

- [x] **Step 1.1: Reproduce `tref_tfunc.wast` failure**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
WASMTIME_BACKTRACE_DETAILS=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/tref_tfunc.wast -- --format terse
```

Expected before the fix: FAIL with `wasm trap: uninitialized element` at the `tcall_indirect` assertion.

- [x] **Step 1.2: Reproduce `tarray_init_elem.wast` failure**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
WASMTIME_BACKTRACE_DETAILS=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/tarray_init_elem.wast -- --format terse
```

Expected before the fix: FAIL with `wasm trap: uninitialized element` after `ttable.set` followed by `tcall_indirect`.

### Task 2: Route Transactional Table Indirect Calls Through the Overlay

**Files:**
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs`

- [x] **Step 2.1: Add a transaction-table overlay flag to indirect-call lowering**

Derive the overlay decision in `translate_call_indirect` and
`translate_return_call_indirect`, then thread the boolean through
`Call::indirect_call`.

The flag is true exactly when `module.transaction_objects.is_ttable(table_index)` is true.

- [x] **Step 2.2: Load `VMFuncRef` through `transaction_ttable_get` for transactional tables**

In `Call::check_and_load_code_and_callee_vmctx`, load `funcref_ptr` with:

```rust
self.env
    .translate_transaction_ttable_get(self.builder, table_index, callee)?
```

when the overlay flag is set. Keep the existing `table_get_funcref` path unchanged for ordinary tables.

- [x] **Step 2.3: Preserve existing signature and null checks**

Reuse the existing `check_indirect_call_type_signature` and `load_code_and_vmctx` logic after the source of `funcref_ptr` is selected. Null staged entries must still trap as indirect-call-to-null, and bad signatures must still trap as bad signatures.

### Task 3: Verify the Focused Fix

**Files:**
- Modify: `docs/shisoft/2026-06-16-wave8a-tcall-indirect-overlay-plan.md`

- [x] **Step 3.1: Run the two focused WAST regressions**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/tref_tfunc.wast -- --format terse

CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/tarray_init_elem.wast -- --format terse
```

Expected after the fix: both pass.

- [x] **Step 3.2: Run focused transaction runtime regression tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
cargo test -p wasmtime transaction_ttable_set_is_private_until_commit --lib -- --format terse
```

Expected after the fix: pass.

- [x] **Step 3.3: Run formatting and diff checks**

Run:

```bash
cargo fmt --check
git diff --check
```

Expected after the fix: both pass.

- [x] **Step 3.4: Run the transactional indirect-call suites**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/tcall_indirect.wast -- --format terse

CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
WASMTIME_TEST_TRANSACTION_WAST=1 \
cargo test -p wasmtime-cli --test wast \
  transaction-proposal/simple-transactions/return_tcall_indirect.wast -- --format terse
```

Expected after the fix: both pass.

### Task 4: Resume the Wave 8 Stabilization Gate

**Files:**
- Modify: `docs/shisoft/2026-06-16-wave8-stabilization-gate-plan.md`

- [x] **Step 4.1: Re-run the full WAST suite with transactional WAST enabled**

Run:

```bash
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target \
CARGO_INCREMENTAL=0 \
WASMTIME_TEST_TRANSACTION_WAST=1 \
CARGO_BUILD_JOBS=2 \
python3 ./ci/run-tests.py --locked \
  --exclude=wasi-preview1-component-adapter \
  -- --format terse
```

Expected after the fix: no WAST failures. If a new failure appears, record it as the next Wave 8 blocker instead of marking this wave complete.
