# Transaction CC Module CFG Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move transaction concurrency-control policy implementations into per-policy modules and compile only the selected policy module.

**Architecture:** Keep `TransactionConcurrencyControl` as the shared policy contract in `concurrency/mod.rs`. Split each policy into its own `concurrency/*.rs` file, gate modules and exports with the matching Cargo feature, and make `ConcurrencyControlState` wrap a concrete feature-selected policy rather than an enum with every policy. Test builds keep direct policy tests feature-gated and rely on the feature matrix to compile every policy.

**Tech Stack:** Rust modules, Cargo `cfg(feature = "...")`, transaction runtime unit tests, `cargo test`, `cargo fmt`.

---

## File Structure

- Rename/split: `crates/wasmtime/src/runtime/transaction/concurrency.rs` into `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/lock_based.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/no_wait_abort.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/wound_wait.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/wait_die.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/strict_2pl.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs`
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/timestamp_ordering.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

## Task 1: Characterize Existing Behavior

- [ ] **Step 1: Run selected focused tests before refactor**

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
```

Expected: the contract test passes and the default transaction suite passes.

## Task 2: Split Policy Implementations

- [ ] **Step 1: Move shared declarations into `concurrency/mod.rs`**

Keep these definitions in the shared module:

- timestamp helper functions
- `TransactionConflictAction`
- `TransactionConcurrencyControl`
- selected-policy type alias
- `ConcurrencyControlState`
- shared helper `action_from_aborted_transaction`

- [ ] **Step 2: Move each policy block into its own file**

Move the existing contiguous policy code blocks without semantic changes:

- `LockBased` and `LockBasedConflictKind` to `lock_based.rs`
- `NoWaitAbort` and `NoWaitAbortConflictKind` to `no_wait_abort.rs`
- `WoundWait` and `WoundWaitConflictKind` to `wound_wait.rs`
- `WaitDie` and `WaitDieConflictKind` to `wait_die.rs`
- `StrictTwoPhaseLocking` and `StrictTwoPhaseLockingConflictKind` to `strict_2pl.rs`
- `OptimisticValidation` and `OptimisticValidationConflictKind` to `optimistic_validation.rs`
- `TimestampOrdering` and `TimestampOrderingConflictKind` to `timestamp_ordering.rs`

Each policy module imports shared items from `super`.

## Task 3: Compile Only The Selected Policy

- [ ] **Step 1: Gate policy modules and aliases**

Use one selected concrete policy alias:

```rust
#[cfg(feature = "transaction-cc-lockbased")]
mod lock_based;
#[cfg(feature = "transaction-cc-lockbased")]
pub(crate) use lock_based::LockBased as SelectedConcurrencyControl;
```

Repeat for every policy feature. Export test-only policy types only behind their matching feature.

- [ ] **Step 2: Replace enum wrapper with concrete wrapper**

Change `ConcurrencyControlState` to:

```rust
#[derive(Debug, Default)]
pub(crate) struct ConcurrencyControlState {
    policy: SelectedConcurrencyControl,
}
```

Forward methods directly to `self.policy`.

- [ ] **Step 3: Keep explicit `for_config` validation**

Keep `ConcurrencyControlState::for_config(policy)` for tests and config plumbing. It should assert that the requested policy matches the compile-time selected policy, then return `Self::default()`.

## Task 4: Update Tests For Feature-Scoped Policy Types

- [ ] **Step 1: Gate direct policy tests**

Add `#[cfg(feature = "...")]` to each direct policy test so test builds only reference compiled policy modules.

- [ ] **Step 2: Update the generic contract test**

Run the contract against `ConcurrencyControlState::default()` or the selected policy type only. The full feature matrix remains responsible for compiling every policy.

- [ ] **Step 3: Update test helper branches**

Replace enum variant matches in tests with helper methods on `ConcurrencyControlState` where needed, so tests do not reference disabled variants.

## Task 5: Verify And Commit

- [ ] **Step 1: Run default verification**

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 2: Run feature matrix**

Run one command per feature:

```bash
for feature in \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering \
  transaction-cc-lockbased
do
  cargo test -p wasmtime --no-default-features \
    --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,${feature}" \
    --lib transaction:: -- --format terse
done
```

Expected: every policy feature test run passes.

- [ ] **Step 3: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Gate transaction concurrency-control policy modules"
```
