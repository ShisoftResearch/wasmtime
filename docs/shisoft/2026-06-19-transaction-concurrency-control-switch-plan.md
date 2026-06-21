# Transaction Concurrency Control Switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a second transaction concurrency-control scheme and select the active scheme with Cargo feature flags.

**Architecture:** Keep the current `LockBased` implementation as the default branch behavior. Add a `NoWaitAbort` implementation that shares the same `GranuleId`, version, and release machinery but never preempts an existing writer. Route `TransactionState` and `TransactionRegionRuntime` through a small selected-policy wrapper so local and shared-runtime transactions use the same compile-time-selected policy.

**Tech Stack:** Rust, Cargo features, Wasmtime transaction runtime, existing `TransactionConcurrencyControl` trait, `cargo test`.

---

## Current State

The runtime currently has the shape we need, but only one concrete policy is wired:

- `crates/wasmtime/src/runtime/transaction/config.rs`
  - `ConcurrencyControl` currently contains only `LockBased`.
  - `TransactionConfig::default()` always selects `ConcurrencyControl::LockBased`.
- `crates/wasmtime/src/runtime/transaction/concurrency.rs`
  - `TransactionConcurrencyControl` has read/write acquire and release hooks.
  - `LockBased` implements optimistic reads and pessimistic writes.
  - `LockBased` uses transaction-id priority: a lower transaction id can preempt and abort a higher-id owner.
- `crates/wasmtime/src/runtime/transaction/state.rs`
  - Store-local runtime state embeds `locks: LockBased`.
- `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - Shared host-thread runtime embeds `locks: LockBased` under `LockAuthorityState`.
- `crates/wasmtime/Cargo.toml`
  - `transaction` is default-on in this branch.
  - There is no concurrency-control feature flag yet.

## Recommended Next Scheme

Start with `NoWaitAbort`.

Semantics:

- Reads are optimistic and record the version observed at read time.
- Writes are pessimistic and acquire ownership of the granule.
- If a granule is owned by another transaction, the contender fails immediately.
- No transaction preempts another transaction based on id.
- Commit still validates optimistic reads against current versions.
- Abort/commit releases ownership and read-version tracking.

Why this is the next easy target:

- It exercises the same runtime paths as `LockBased`.
- It is meaningfully different from `LockBased`: older transactions cannot wound younger owners.
- It does not require redo/undo log format changes.
- It does not require waiting queues, deadlock detection, timestamps beyond the existing `TransactionId`, or MVCC snapshots.
- It gives us a feature-switch framework before attempting harder schemes.

Schemes to defer until after this switch exists:

- `WaitDie` or `WoundWait`: still lock-based, but needs clearer retry/wait policy and scheduling behavior.
- `StrictTwoPhaseLocking`: close to `NoWaitAbort`, but should decide whether reads also acquire shared locks.
- `OptimisticValidationOnly`: larger change because writes should avoid pessimistic ownership until commit.
- MVCC/snapshot isolation: useful research target, but it needs multi-version granule storage, version garbage collection, and conflict rules for object roots and tables.

## Feature Flag Shape

Use feature flags for the compile-time default policy:

```toml
# crates/wasmtime/Cargo.toml

transaction = [
  "runtime",
  "std",
  "gc",
  "wasmtime-environ/transaction",
  "wasmtime-cranelift?/transaction",
]

transaction-cc-lockbased = ["transaction"]
transaction-cc-nowait-abort = ["transaction"]
```

The branch default should include this exact entry:

```toml
"transaction-cc-lockbased",
```

At the root CLI package, mirror the feature names:

```toml
# Cargo.toml

transaction-cc-lockbased = ["wasmtime/transaction-cc-lockbased"]
transaction-cc-nowait-abort = ["wasmtime/transaction-cc-nowait-abort"]
```

Add a compile-time guard in `crates/wasmtime/src/runtime/transaction/config.rs`:

```rust
#[cfg(all(
    feature = "transaction",
    feature = "transaction-cc-lockbased",
    feature = "transaction-cc-nowait-abort"
))]
compile_error!(
    "select exactly one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);

#[cfg(all(
    feature = "transaction",
    not(any(
        feature = "transaction-cc-lockbased",
        feature = "transaction-cc-nowait-abort"
    ))
))]
compile_error!(
    "transaction requires one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);
```

This means the default branch keeps `LockBased`. To switch for experiments:

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib transaction:: -- --format terse
```

If that exact feature set is too narrow for a specific test command, add the missing Wasmtime feature to the command instead of weakening the concurrency-control guard.

## Files

- Modify: `crates/wasmtime/Cargo.toml`
  - Add `transaction-cc-lockbased` and `transaction-cc-nowait-abort`.
  - Move the default branch selection from `transaction` to `transaction-cc-lockbased`.
- Modify: `Cargo.toml`
  - Add root feature aliases for both policy flags.
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
  - Add feature guards.
  - Add `ConcurrencyControl::NoWaitAbort`.
  - Make the default policy follow the selected Cargo feature.
  - Add a test-only constructor or setter for explicit policy selection.
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
  - Add `NoWaitAbort`.
  - Add `ConcurrencyControlState`, a concrete wrapper enum used by runtime state.
  - Implement the common concurrency-control methods for the wrapper.
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
  - Replace `locks: LockBased` with selected concurrency-control state.
  - Route local transaction operations through the wrapper.
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - Replace shared `locks: LockBased` with selected concurrency-control state.
  - Preserve the existing terminal-owner conflict check.
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Add unit tests for `NoWaitAbort`.
  - Gate `LockBased`-specific preemption tests under `transaction-cc-lockbased`.
  - Add feature-specific config tests.
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
  - Keep the generic threaded conflict tests policy-neutral.
  - Add only policy-specific assertions if the observable winner must differ.
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Document feature-selected concurrency control.
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record the selected policy switch and test commands.

---

## Task 1: Cargo Feature Guard

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Test: `cargo check -p wasmtime --no-default-features --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort"`

- [ ] **Step 1: Write failing config tests**

Add tests near the existing transaction config tests in `crates/wasmtime/src/runtime/transaction/tests.rs`:

```rust
#[test]
#[cfg(feature = "transaction-cc-lockbased")]
fn transaction_config_default_concurrency_is_lock_based_under_feature() {
    assert_eq!(
        TransactionConfig::default().concurrency_control(),
        ConcurrencyControl::LockBased
    );
}

#[test]
#[cfg(feature = "transaction-cc-nowait-abort")]
fn transaction_config_default_concurrency_is_nowait_abort_under_feature() {
    assert_eq!(
        TransactionConfig::default().concurrency_control(),
        ConcurrencyControl::NoWaitAbort
    );
}
```

- [ ] **Step 2: Run tests to verify the new feature case fails**

Run:

```bash
cargo test -p wasmtime transaction_config_default_concurrency_is_nowait_abort_under_feature \
  --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib -- --format terse
```

Expected: fail to compile because `transaction-cc-nowait-abort` and `ConcurrencyControl::NoWaitAbort` do not exist.

- [ ] **Step 3: Add the Cargo features**

In `crates/wasmtime/Cargo.toml`, keep `transaction` as the base feature and add the policy features:

```toml
transaction = [
  "runtime",
  "std",
  "gc",
  "wasmtime-environ/transaction",
  "wasmtime-cranelift?/transaction",
]

transaction-cc-lockbased = ["transaction"]
transaction-cc-nowait-abort = ["transaction"]
```

Update the `default` feature list in `crates/wasmtime/Cargo.toml` to include:

```toml
"transaction-cc-lockbased",
```

In the root `Cargo.toml`, add aliases:

```toml
transaction-cc-lockbased = ["wasmtime/transaction-cc-lockbased"]
transaction-cc-nowait-abort = ["wasmtime/transaction-cc-nowait-abort"]
```

- [ ] **Step 4: Add config feature guards and enum variant**

In `crates/wasmtime/src/runtime/transaction/config.rs`, add:

```rust
#[cfg(all(
    feature = "transaction",
    feature = "transaction-cc-lockbased",
    feature = "transaction-cc-nowait-abort"
))]
compile_error!(
    "select exactly one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);

#[cfg(all(
    feature = "transaction",
    not(any(
        feature = "transaction-cc-lockbased",
        feature = "transaction-cc-nowait-abort"
    ))
))]
compile_error!(
    "transaction requires one transaction concurrency-control feature: \
     transaction-cc-lockbased or transaction-cc-nowait-abort"
);
```

Change the enum:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
    NoWaitAbort,
}
```

Add:

```rust
impl ConcurrencyControl {
    pub(crate) const fn default_for_build() -> Self {
        #[cfg(feature = "transaction-cc-nowait-abort")]
        {
            return Self::NoWaitAbort;
        }

        #[cfg(feature = "transaction-cc-lockbased")]
        {
            return Self::LockBased;
        }
    }
}
```

Change `TransactionConfig::default()`:

```rust
concurrency_control: ConcurrencyControl::default_for_build(),
```

Add a setter:

```rust
pub(crate) fn with_concurrency_control(concurrency_control: ConcurrencyControl) -> Result<Self> {
    let mut config = Self::default();
    config.set_concurrency_control(concurrency_control)?;
    Ok(config)
}

fn set_concurrency_control(&mut self, concurrency_control: ConcurrencyControl) -> Result<()> {
    self.concurrency_control = concurrency_control;
    Ok(())
}
```

- [ ] **Step 5: Run the config tests**

Run default:

```bash
cargo test -p wasmtime transaction_config_default_concurrency_is_lock_based_under_feature \
  --lib -- --format terse
```

Expected: pass.

Run alternate:

```bash
cargo test -p wasmtime transaction_config_default_concurrency_is_nowait_abort_under_feature \
  --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib -- --format terse
```

Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
  crates/wasmtime/src/runtime/transaction/config.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add transaction concurrency-control feature flags"
```

---

## Task 2: Add `NoWaitAbort`

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Test: `cargo test -p wasmtime no_wait_abort --lib -- --format terse`

- [ ] **Step 1: Write failing `NoWaitAbort` tests**

Add tests near the existing `lock_based_*` tests:

```rust
#[test]
fn no_wait_abort_allows_shared_reads_and_writer_upgrade() {
    let mut locks = NoWaitAbort::default();
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    locks.record_read_for_test(first, granule, 5).unwrap();
    locks.record_read_for_test(second, granule, 5).unwrap();
    locks.acquire_write_for_test(second, granule, 5).unwrap();
}

#[test]
fn no_wait_abort_writer_excludes_other_transactions_without_preemption() {
    let mut locks = NoWaitAbort::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    locks.acquire_write_for_test(younger, granule, 3).unwrap();

    let read_error = locks
        .record_read_result_for_test(older, granule, 3)
        .unwrap_err();
    assert_eq!(read_error, NoWaitAbortConflictKindForTest::ReadOwnedByOther);

    let write_error = locks
        .acquire_write_result_for_test(older, granule, 3)
        .unwrap_err();
    assert_eq!(write_error, NoWaitAbortConflictKindForTest::WriteOwnedByOther);
    assert_eq!(locks.owner_for_test(granule), Some(younger));
}

#[test]
fn no_wait_abort_validates_optimistic_reads_at_commit() {
    let mut locks = NoWaitAbort::default();
    let reader = TransactionId::from_raw(1);
    let writer = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };

    locks.record_read_for_test(reader, granule, 7).unwrap();
    locks.acquire_write_for_test(writer, granule, 7).unwrap();
    locks.abort_for_test(writer);

    let error = locks
        .validate_read_result_for_test(reader, granule, 8)
        .unwrap_err();
    assert_eq!(error, NoWaitAbortConflictKindForTest::ReadVersionMismatch);
}

#[test]
fn no_wait_abort_abort_releases_owned_granules() {
    let mut locks = NoWaitAbort::default();
    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 0,
    };

    locks
        .acquire_write_for_test(transaction, granule, 4)
        .unwrap();
    assert_eq!(locks.owner_for_test(granule), Some(transaction));

    locks.abort_for_test(transaction);

    assert_eq!(locks.owner_for_test(granule), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime no_wait_abort --lib -- --format terse
```

Expected: fail to compile because `NoWaitAbort` is not defined.

- [ ] **Step 3: Implement `NoWaitAbort`**

Add to `crates/wasmtime/src/runtime/transaction/concurrency.rs`:

```rust
#[derive(Debug, Default)]
pub(crate) struct NoWaitAbort {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NoWaitAbortConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl NoWaitAbortConflictKind {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::ReadOwnedByOther => {
                "transaction read conflict: granule is owned by another transaction"
            }
            Self::ReadVersionMismatch => {
                "transaction read conflict: optimistic read version changed"
            }
            Self::WriteOwnedByOther => {
                "transaction write conflict: granule is owned by another transaction"
            }
            Self::WriteVersionMismatch => {
                "transaction write conflict: optimistic read version changed"
            }
        }
    }
}

#[cfg(test)]
pub(crate) use self::NoWaitAbortConflictKind as NoWaitAbortConflictKindForTest;

impl NoWaitAbort {
    pub(crate) fn record_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<Option<TransactionId>> {
        Self::map_conflict_result(self.record_read_typed(transaction, granule, version))
    }

    pub(crate) fn acquire_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        Self::map_conflict_result(self.acquire_write_typed(transaction, granule, current_version))
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        Self::map_conflict_result(self.validate_read_typed(transaction, granule, current_version))
    }

    pub(crate) fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        if let Some(version) = self.read_versions.get_mut(&(transaction, granule)) {
            *version = current_version;
        }
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.owners.retain(|_, owner| *owner != transaction);
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }

    fn map_conflict_result<T>(
        result: core::result::Result<T, NoWaitAbortConflictKind>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(kind) => bail!(kind.message()),
        }
    }

    fn record_read_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<Option<TransactionId>, NoWaitAbortConflictKind> {
        self.reject_other_writer(transaction, granule, false)?;

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(NoWaitAbortConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(None)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, NoWaitAbortConflictKind> {
        self.reject_other_writer(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(NoWaitAbortConflictKind::WriteVersionMismatch);
        }

        self.owners.insert(granule, transaction);
        Ok(None)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(NoWaitAbortConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(NoWaitAbortConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn reject_other_writer(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<(), NoWaitAbortConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(if is_write {
                NoWaitAbortConflictKind::WriteOwnedByOther
            } else {
                NoWaitAbortConflictKind::ReadOwnedByOther
            });
        }
        Ok(())
    }
}
```

Add test helpers matching the current `LockBased` helper style:

```rust
#[cfg(test)]
impl NoWaitAbort {
    pub(crate) fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        self.record_read(transaction, granule, version)?;
        Ok(())
    }

    pub(crate) fn acquire_write_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.acquire_write(transaction, granule, current_version)?;
        Ok(())
    }

    pub(crate) fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    pub(crate) fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), NoWaitAbortConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}
```

Add trait implementation:

```rust
impl TransactionConcurrencyControl for NoWaitAbort {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.record_read(transaction, granule, current_version)
    }

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.acquire_write(transaction, granule, current_version)
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        NoWaitAbort::release_transaction(self, transaction);
    }
}
```

- [ ] **Step 4: Run `NoWaitAbort` tests**

Run:

```bash
cargo test -p wasmtime no_wait_abort --lib -- --format terse
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add no-wait abort concurrency policy"
```

---

## Task 3: Runtime Policy Wrapper

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Test: `cargo test -p wasmtime --lib transaction:: -- --format terse`

- [ ] **Step 1: Write failing selected-policy test**

Add a unit test near the concurrency tests:

```rust
#[test]
#[cfg(feature = "transaction-cc-nowait-abort")]
fn selected_concurrency_control_uses_nowait_abort_semantics() {
    let mut policy = ConcurrencyControlState::for_config(ConcurrencyControl::NoWaitAbort);
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy
        .acquire_granule_write(younger, granule, 0)
        .unwrap();
    let err = policy
        .acquire_granule_write(older, granule, 0)
        .unwrap_err();
    assert!(
        err.to_string().contains("transaction write conflict"),
        "{err:?}"
    );
    assert_eq!(policy.owner_for_granule(granule), Some(younger));
}
```

- [ ] **Step 2: Run the selected-policy test**

Run:

```bash
cargo test -p wasmtime selected_concurrency_control_uses_nowait_abort_semantics \
  --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib -- --format terse
```

Expected: fail to compile because `ConcurrencyControlState` does not exist.

- [ ] **Step 3: Add `ConcurrencyControlState`**

In `crates/wasmtime/src/runtime/transaction/concurrency.rs`, import `ConcurrencyControl`:

```rust
use super::{ConcurrencyControl, GranuleId, TransactionId};
```

Add:

```rust
#[derive(Debug)]
pub(crate) enum ConcurrencyControlState {
    LockBased(LockBased),
    NoWaitAbort(NoWaitAbort),
}

impl Default for ConcurrencyControlState {
    fn default() -> Self {
        Self::for_config(ConcurrencyControl::default_for_build())
    }
}

impl ConcurrencyControlState {
    pub(crate) fn for_config(policy: ConcurrencyControl) -> Self {
        match policy {
            ConcurrencyControl::LockBased => Self::LockBased(LockBased::default()),
            ConcurrencyControl::NoWaitAbort => Self::NoWaitAbort(NoWaitAbort::default()),
        }
    }

    pub(crate) fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        match self {
            Self::LockBased(policy) => {
                policy.acquire_granule_read(transaction, granule, current_version)
            }
            Self::NoWaitAbort(policy) => {
                policy.acquire_granule_read(transaction, granule, current_version)
            }
        }
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        match self {
            Self::LockBased(policy) => {
                policy.acquire_granule_write(transaction, granule, current_version)
            }
            Self::NoWaitAbort(policy) => {
                policy.acquire_granule_write(transaction, granule, current_version)
            }
        }
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        match self {
            Self::LockBased(policy) => policy.validate_read(transaction, granule, current_version),
            Self::NoWaitAbort(policy) => policy.validate_read(transaction, granule, current_version),
        }
    }

    pub(crate) fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        match self {
            Self::LockBased(policy) => {
                policy.refresh_read_version(transaction, granule, current_version);
            }
            Self::NoWaitAbort(policy) => {
                policy.refresh_read_version(transaction, granule, current_version);
            }
        }
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        match self {
            Self::LockBased(policy) => policy.release_transaction(transaction),
            Self::NoWaitAbort(policy) => policy.release_transaction(transaction),
        }
    }

    pub(crate) fn release_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
        self.release_transaction(transaction);
        Ok(())
    }

    pub(crate) fn owner_for_granule(&self, granule: GranuleId) -> Option<TransactionId> {
        match self {
            Self::LockBased(policy) => policy.owner_for_granule(granule),
            Self::NoWaitAbort(policy) => policy.owner_for_granule(granule),
        }
    }

    pub(crate) fn read_granules_for_transaction(
        &self,
        transaction: TransactionId,
    ) -> Vec<GranuleId> {
        match self {
            Self::LockBased(policy) => policy
                .read_versions
                .keys()
                .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
                .collect(),
            Self::NoWaitAbort(policy) => policy
                .read_versions
                .keys()
                .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
                .collect(),
        }
    }
}
```

- [ ] **Step 4: Wire `TransactionState`**

In `crates/wasmtime/src/runtime/transaction/state.rs`, change:

```rust
pub(super) locks: LockBased,
```

to:

```rust
pub(super) concurrency: ConcurrencyControlState,
```

Change default initialization:

```rust
concurrency: ConcurrencyControlState::default(),
```

Change local accessors:

```rust
self.concurrency
    .acquire_granule_read(transaction, granule, current_version)
```

```rust
self.concurrency
    .acquire_granule_write(transaction, granule, current_version)
```

```rust
self.concurrency.refresh_read_version(
    transaction,
    granule,
    current_version,
);
```

```rust
self.concurrency
    .validate_read(transaction, granule, current_version)
```

```rust
self.concurrency.release_transaction_result(transaction)
```

Change `active_read_granules()` local branch:

```rust
Ok(self.concurrency.read_granules_for_transaction(transaction))
```

- [ ] **Step 5: Wire `TransactionRegionRuntime`**

In `crates/wasmtime/src/runtime/transaction/region_runtime.rs`, change:

```rust
locks: LockBased,
```

to:

```rust
concurrency: ConcurrencyControlState,
```

Change default initialization:

```rust
concurrency: ConcurrencyControlState::default(),
```

Change all `runtime.locks.*` calls to the matching `runtime.concurrency.*` method.

Change terminal owner check:

```rust
let Some(owner) = runtime.concurrency.owner_for_granule(granule) else {
    return Ok(());
};
```

- [ ] **Step 6: Run default runtime tests**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
```

Expected: pass.

- [ ] **Step 7: Run alternate runtime tests**

Run:

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib transaction:: -- --format terse
```

Expected: pass or expose tests that assume `LockBased` preemption. If a test asserts lower-id preemption, gate it with `#[cfg(feature = "transaction-cc-lockbased")]` and add a `NoWaitAbort` equivalent.

- [ ] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Wire selected transaction concurrency policy"
```

---

## Task 4: Shared Runtime and Threaded Integration Tests

**Files:**
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Test: `cargo test -p wasmtime --test transaction_persistence shared_runtime_threaded -- --format terse`

- [ ] **Step 1: Keep existing threaded tests policy-neutral**

The current tests should remain valid for both policies:

- `shared_runtime_threaded_tmemory_commits_recover_together`
- `shared_runtime_threaded_object_graphs_recover_both_roots`
- `shared_runtime_threaded_tmemory_conflict_recovers_only_committed_version`
- `shared_runtime_threaded_mixed_conflict_recovers_only_committed_version`

They should assert:

```rust
fn assert_transaction_conflict(error: &str) {
    assert!(
        error.contains("transaction read conflict") || error.contains("transaction write conflict"),
        "{error}"
    );
}
```

They should not assert which transaction id wins unless the test is gated to one policy.

- [ ] **Step 2: Add an explicit `NoWaitAbort` winner test**

Add this test in `crates/wasmtime/tests/transaction_persistence.rs`:

```rust
#[test]
#[cfg(feature = "transaction-cc-nowait-abort")]
fn shared_runtime_nowait_abort_does_not_preempt_existing_owner() -> Result<()> {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-nowait-owner.tmemory");
    let tx_log_path = dir.path().join("threaded-nowait-owner.txlog");
    let engine = transaction_persistence_engine()?;
    let module = blocking_tmemory_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime, 1)?;

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let first = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let gate = gate.clone();
        thread::spawn(move || -> Result<()> {
            let mut store = shared_store(&engine, &runtime)?;
            let gate = gate.clone();
            let pause = Func::wrap(&mut store, move || {
                let (lock, ready) = &*gate;
                let mut released = lock.lock().unwrap();
                *released = true;
                ready.notify_one();
                while *released {
                    released = ready.wait(released).unwrap();
                }
            });
            let instance = Instance::new(&mut store, &module, &[pause.into()])?;
            instance
                .get_typed_func::<i32, ()>(&mut store, "write")?
                .call(&mut store, 0x1122_3344)?;
            Ok(())
        })
    };

    let (lock, ready) = &*gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        released = ready.wait(released).unwrap();
    }

    let mut second_store = shared_store(&engine, &runtime)?;
    let second_pause = Func::wrap(&mut second_store, || {});
    let second_instance = Instance::new(&mut second_store, &module, &[second_pause.into()])?;
    let err = second_instance
        .get_typed_func::<i32, ()>(&mut second_store, "write")?
        .call(&mut second_store, 0x5566_7788)
        .unwrap_err();
    let err = format!("{err:?}");
    assert_transaction_conflict(&err);

    *released = false;
    ready.notify_one();
    drop(released);
    first.join().unwrap()?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &0x1122_3344u32.to_le_bytes())?;

    Ok(())
}
```

- [ ] **Step 3: Run default threaded tests**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence shared_runtime_threaded -- --format terse
```

Expected: pass.

- [ ] **Step 4: Run alternate threaded tests**

Run:

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --test transaction_persistence shared_runtime -- --format terse
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Test threaded transactions under selected concurrency policy"
```

---

## Task 5: Documentation

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Test: `rg -n "NoWaitAbort|transaction-cc-nowait-abort|transaction-cc-lockbased" docs/shisoft`

- [ ] **Step 1: Update the core design doc**

Add this section near the existing concurrency-control design:

```markdown
### Compile-Time Concurrency-Control Selection

The transaction branch keeps concurrency control behind the default-on
`transaction` feature and selects the concrete policy with one
`transaction-cc-*` feature.

The default policy is `transaction-cc-lockbased`, which preserves the current
Wizard-aligned lock-based behavior: optimistic reads, pessimistic writes, and
transaction-id priority where an older transaction can abort a younger owner.

The first alternate policy is `transaction-cc-nowait-abort`. It uses the same
`GranuleId` ownership and version validation state, but a transaction never
preempts an existing owner. If another transaction owns the granule, the
contender receives a transaction conflict and aborts. This policy is useful as
the smallest meaningful feature-selected alternative because it does not change
durable log format, object identity, tmemory undo records, or recovery.

Only one `transaction-cc-*` feature may be selected at a time. Shared host-thread
transaction runtimes and store-local transaction runtimes must use the same
selected policy.
```

- [ ] **Step 2: Update the implementation log**

Add this entry:

```markdown
## Transaction Concurrency-Control Feature Switch

Added a compile-time policy switch for transaction concurrency control:

- `transaction-cc-lockbased`: default branch behavior.
- `transaction-cc-nowait-abort`: no-wait ownership conflicts, with no
  transaction-id preemption.

Both policies operate over `GranuleId`, share optimistic read-version
validation, and release all owned/read granules on commit or abort. The switch
is intentionally compile-time for now so experiments can compare policy behavior
without adding public embedding API.

Verification commands:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --test transaction_persistence shared_runtime_threaded -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib transaction:: -- --format terse
```
```

- [ ] **Step 3: Search docs for policy names**

Run:

```bash
rg -n "NoWaitAbort|transaction-cc-nowait-abort|transaction-cc-lockbased" docs/shisoft
```

Expected: matches in the core design doc, implementation log, and this handoff plan.

- [ ] **Step 4: Commit**

```bash
git add docs/shisoft/transactional-wasm-runtime-core-design.md \
  docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document transaction concurrency-control policy switch"
```

---

## Task 6: Final Verification Matrix

**Files:**
- Read: changed files from Tasks 1-5
- Test: all commands below

- [ ] **Step 1: Default branch checks**

Run:

```bash
cargo fmt --all -- --check
git diff --check
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo run --example transaction-multithread
```

Expected:

- format check exits 0
- whitespace check exits 0
- transaction unit tests pass
- `transaction_persistence` integration tests pass
- example prints recovered tmemory and persistent object leaf values

- [ ] **Step 2: Alternate policy checks**

Run:

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --lib transaction:: -- --format terse

cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,transaction-cc-nowait-abort" \
  --test transaction_persistence shared_runtime -- --format terse
```

Expected:

- alternate policy unit tests pass
- threaded shared-runtime tests pass
- `NoWaitAbort` winner behavior matches the existing-owner-wins rule

- [ ] **Step 3: Commit final fixes**

If verification required small fixes, commit them:

```bash
git add crates/wasmtime/Cargo.toml Cargo.toml \
  crates/wasmtime/src/runtime/transaction/config.rs \
  crates/wasmtime/src/runtime/transaction/concurrency.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-runtime-core-design.md \
  docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Verify selected transaction concurrency policies"
```

---

## Self-Review

Spec coverage:

- Adds a second concrete concurrency-control scheme: `NoWaitAbort`.
- Adds compiler-feature selection: `transaction-cc-lockbased` and `transaction-cc-nowait-abort`.
- Keeps current behavior default-on for this branch.
- Includes unit, shared-runtime, and threaded integration tests.
- Gives next easy-target recommendation and identifies harder schemes to defer.

Placeholder scan:

- The plan contains no open placeholder steps.
- Every code-changing task includes explicit files, snippets, commands, and expected outcomes.

Type consistency:

- `ConcurrencyControl::NoWaitAbort`, `NoWaitAbort`, `NoWaitAbortConflictKindForTest`, and `ConcurrencyControlState` are used consistently.
- Store-local and shared runtime both route through `ConcurrencyControlState`.
