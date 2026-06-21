# Transaction Concurrency-Control Trait Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `TransactionConcurrencyControl` the full single-version policy contract so future timestamp policies can be added by implementing one trait.

**Architecture:** Keep `ConcurrencyControlState` as the compile-time-selected enum wrapper, but make it delegate through `TransactionConcurrencyControl` for all runtime policy operations. No transaction behavior should change in this slice.

**Tech Stack:** Rust, Cargo features, Wasmtime transaction runtime, `cargo test`.

---

## Files

- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
  - Expand `TransactionConcurrencyControl`.
  - Route `ConcurrencyControlState` through the trait.
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Add a generic contract test that compiles only when the trait exposes the full policy surface.

## Task 1: Trait Contract Expansion

- [ ] **Step 1: Write the failing generic contract test**

Add a test near the direct concurrency-policy tests:

```rust
#[test]
fn transaction_concurrency_control_trait_covers_runtime_policy_contract() {
    fn assert_contract<T: super::concurrency::TransactionConcurrencyControl>(
        policy: &mut T,
        transaction: TransactionId,
        granule: GranuleId,
    ) {
        policy
            .acquire_granule_read(transaction, granule, 1)
            .unwrap();
        policy.refresh_read_version(transaction, granule, 2);
        policy.validate_read(transaction, granule, 2).unwrap();
        assert_eq!(
            policy.read_granules_for_transaction(transaction),
            vec![granule]
        );
        assert_eq!(policy.owner_for_granule(granule), None);
        policy.release_transaction_result(transaction).unwrap();
        assert!(
            policy
                .read_granules_for_transaction(transaction)
                .is_empty()
        );
    }

    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };

    assert_contract(&mut LockBased::default(), transaction, granule);
    assert_contract(&mut NoWaitAbort::default(), transaction, granule);
}
```

- [ ] **Step 2: Run the focused test and verify it fails**

Run:

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract \
  --lib -- --format terse
```

Expected: compile failure because `TransactionConcurrencyControl` does not yet define the full runtime policy methods.

- [ ] **Step 3: Expand the trait**

Add these methods to `TransactionConcurrencyControl`:

```rust
fn validate_read(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<()>;

fn refresh_read_version(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
);

fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
    self.release_transaction(transaction);
    Ok(())
}

fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId>;

fn read_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId>;
```

Implement the new required methods for `LockBased` and `NoWaitAbort` by delegating to their existing inherent methods and maps.

- [ ] **Step 4: Refactor `ConcurrencyControlState` delegation**

Update `ConcurrencyControlState` methods to call the trait methods on each concrete policy. Behavior must remain unchanged.

- [ ] **Step 5: Run verification**

Run:

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract \
  --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.
