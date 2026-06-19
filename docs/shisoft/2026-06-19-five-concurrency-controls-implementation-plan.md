# Five Concurrency Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Wound-Wait, Wait-Die, Strict Two-Phase Locking, Optimistic Validation Only, and Pure Timestamp Ordering as selectable single-version transaction concurrency-control policies.

**Architecture:** Keep `TransactionConcurrencyControl` as the contract for single-version policies and keep `ConcurrencyControlState` as the compile-time selected enum wrapper. Add policies in dependency order: timestamp helpers, Wound-Wait, structured conflict actions, Wait-Die, Strict 2PL, commit-time write validation, Optimistic Validation, timestamp commit metadata, and Pure Timestamp Ordering. MVCC remains a future storage/GC/recovery project and is not implemented here.

**Tech Stack:** Rust, Cargo features, Wasmtime transaction runtime, `BTreeMap`/`BTreeSet`, unit tests in `crates/wasmtime/src/runtime/transaction/tests.rs`, `cargo test`, `cargo check`, `cargo fmt`.

---

## Preconditions

- Work from `/home/shisoft/Code/Research/wasmtime` on branch `transaction`.
- Preserve the Bytecode Alliance AI Tool Use Policy requirements from `AGENTS.md`: do not open Wasmtime PRs, do not comment on PRs/issues, and do not add AI/tool/model co-author annotations.
- The policy-contract refactor in `crates/wasmtime/src/runtime/transaction/concurrency.rs` and `crates/wasmtime/src/runtime/transaction/tests.rs` should be committed before starting new policy work.
- Do not implement MVCC in this plan. Leave extension points only.

## File Structure

- Modify `crates/wasmtime/src/runtime/transaction/concurrency.rs`
  - policy trait
  - timestamp helpers
  - conflict action enum
  - policy structs and conflict kinds
  - `ConcurrencyControlState` variants and delegation
- Modify `crates/wasmtime/src/runtime/transaction/config.rs`
  - Cargo feature guard strings
  - `ConcurrencyControl` variants
  - `default_for_build`
- Modify `crates/wasmtime/src/runtime/transaction/mod.rs`
  - test-only policy and conflict-kind imports
- Modify `crates/wasmtime/src/runtime/transaction/state.rs`
  - structured conflict action handling
  - write-validation hooks
  - commit-success policy hook
- Modify `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - shared-runtime structured conflict action handling
  - write-validation and commit-success forwarding
  - terminal-owner check policy neutrality
- Modify `crates/wasmtime/src/runtime/transaction/tests.rs`
  - direct policy tests
  - selected-policy tests under feature gates
  - shared-runtime behavior tests
- Modify `crates/wasmtime/Cargo.toml`
  - crate-level transaction concurrency-control features
- Modify root `Cargo.toml`
  - workspace CLI feature forwarding

## Shared Verification Commands

Use these after each task unless the task gives a narrower command first:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Use this support-feature set for alternate policy feature tests, replacing the last feature name:

```bash
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-wound-wait" \
  --lib transaction:: -- --format terse
```

---

### Task 1: Commit The Policy-Contract Baseline

**Files:**
- Modify already present: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify already present: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Verify the generic policy-contract test**

Run:

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract --lib -- --format terse
```

Expected: `1 passed`.

- [ ] **Step 2: Verify default transaction tests**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
```

Expected: all transaction tests pass.

- [ ] **Step 3: Verify formatting and whitespace**

Run:

```bash
cargo fmt --all -- --check
git diff --check -- crates/wasmtime/src/runtime/transaction/concurrency.rs crates/wasmtime/src/runtime/transaction/tests.rs
```

Expected: both commands exit successfully.

- [ ] **Step 4: Commit only the trait-contract baseline**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Expand transaction concurrency-control trait contract"
```

Expected: one commit containing only the contract refactor and test.

---

### Task 2: Add Timestamp Helper Terminology

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write the failing timestamp-helper test**

Add this test near the existing policy unit tests in `tests.rs`:

```rust
#[test]
fn transaction_timestamp_helpers_use_transaction_id_order() {
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);

    assert_eq!(
        super::concurrency::timestamp_for_transaction_for_test(older),
        older
    );
    assert!(super::concurrency::is_older_for_test(older, younger));
    assert!(super::concurrency::is_younger_for_test(younger, older));
    assert!(!super::concurrency::is_older_for_test(younger, older));
    assert!(!super::concurrency::is_younger_for_test(older, younger));
}
```

- [ ] **Step 2: Run the helper test to verify it fails**

Run:

```bash
cargo test -p wasmtime transaction_timestamp_helpers_use_transaction_id_order --lib -- --format terse
```

Expected: compile failure because the helper functions do not exist.

- [ ] **Step 3: Add private timestamp helpers**

Add this near the top of `concurrency.rs`, after the `use super::{...};` line:

```rust
type TransactionTimestamp = TransactionId;

fn timestamp_for_transaction(transaction: TransactionId) -> TransactionTimestamp {
    transaction
}

fn is_older(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) < timestamp_for_transaction(owner)
}

fn is_younger(requester: TransactionId, owner: TransactionId) -> bool {
    timestamp_for_transaction(requester) > timestamp_for_transaction(owner)
}

#[cfg(test)]
pub(crate) fn timestamp_for_transaction_for_test(
    transaction: TransactionId,
) -> TransactionTimestamp {
    timestamp_for_transaction(transaction)
}

#[cfg(test)]
pub(crate) fn is_older_for_test(requester: TransactionId, owner: TransactionId) -> bool {
    is_older(requester, owner)
}

#[cfg(test)]
pub(crate) fn is_younger_for_test(requester: TransactionId, owner: TransactionId) -> bool {
    is_younger(requester, owner)
}
```

- [ ] **Step 4: Run the helper test**

Run:

```bash
cargo test -p wasmtime transaction_timestamp_helpers_use_transaction_id_order --lib -- --format terse
```

Expected: `1 passed`.

- [ ] **Step 5: Run shared verification and commit**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add transaction timestamp helper terminology"
```

Expected: tests pass and one helper-only commit is created.

---

### Task 3: Add Wound-Wait

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing direct Wound-Wait tests**

Add these tests near the `NoWaitAbort` direct tests:

```rust
#[test]
fn wound_wait_older_writer_wounds_younger_owner() {
    let mut policy = WoundWait::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(younger, granule, 3).unwrap();
    let wounded = policy.acquire_write(older, granule, 3).unwrap();

    assert_eq!(wounded, Some(younger));
    assert_eq!(policy.owner_for_test(granule), Some(older));
}

#[test]
fn wound_wait_older_reader_wounds_younger_writer() {
    let mut policy = WoundWait::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(younger, granule, 3).unwrap();
    let wounded = policy.record_read(older, granule, 3).unwrap();

    assert_eq!(wounded, Some(younger));
    assert_eq!(policy.owner_for_test(granule), None);
    assert_eq!(policy.read_granules_for_transaction(older), vec![granule]);
}

#[test]
fn wound_wait_younger_requester_conflicts_with_older_owner() {
    let mut policy = WoundWait::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(older, granule, 3).unwrap();

    let read_error = policy
        .record_read_result_for_test(younger, granule, 3)
        .unwrap_err();
    assert_eq!(read_error, WoundWaitConflictKindForTest::ReadOwnedByOther);

    let write_error = policy
        .acquire_write_result_for_test(younger, granule, 3)
        .unwrap_err();
    assert_eq!(write_error, WoundWaitConflictKindForTest::WriteOwnedByOther);
}
```

- [ ] **Step 2: Run direct tests to verify they fail**

Run:

```bash
cargo test -p wasmtime wound_wait_ --lib -- --format terse
```

Expected: compile failure because `WoundWait` and `WoundWaitConflictKindForTest` do not exist.

- [ ] **Step 3: Add feature flags**

In `crates/wasmtime/Cargo.toml`, add:

```toml
transaction-cc-wound-wait = ["transaction"]
```

In root `Cargo.toml`, add:

```toml
transaction-cc-wound-wait = ["wasmtime/transaction-cc-wound-wait"]
```

- [ ] **Step 4: Update policy selection config**

In `config.rs`, extend the compile guards and default selection with `transaction-cc-wound-wait`. The resulting feature guard should include this feature in both the "more than one" and "none selected" checks. Add:

```rust
pub(crate) enum ConcurrencyControl {
    LockBased,
    NoWaitAbort,
    WoundWait,
}
```

Add this branch before the lock-based branch in `default_for_build`:

```rust
#[cfg(feature = "transaction-cc-wound-wait")]
{
    return Self::WoundWait;
}
```

- [ ] **Step 5: Add the Wound-Wait policy type**

In `concurrency.rs`, add this policy after `NoWaitAbort`:

```rust
#[derive(Debug, Default)]
pub(crate) struct WoundWait {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WoundWaitConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl WoundWaitConflictKind {
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
pub(crate) use self::WoundWaitConflictKind as WoundWaitConflictKindForTest;
```

Add inherent methods matching `LockBased`, but use `is_older(transaction, owner)` inside conflict resolution:

```rust
impl WoundWait {
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
        result: core::result::Result<T, WoundWaitConflictKind>,
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
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let wounded = self.resolve_writer_conflict_typed(transaction, granule, false)?;
        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(WoundWaitConflictKind::ReadVersionMismatch);
                }
            }
        }
        Ok(wounded)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let wounded = self.resolve_writer_conflict_typed(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WoundWaitConflictKind::WriteVersionMismatch);
        }
        self.owners.insert(granule, transaction);
        Ok(wounded)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(WoundWaitConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(WoundWaitConflictKind::ReadVersionMismatch);
        }
        Ok(())
    }

    fn resolve_writer_conflict_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<Option<TransactionId>, WoundWaitConflictKind> {
        let Some(owner) = self.owners.get(&granule).copied() else {
            return Ok(None);
        };
        if owner == transaction {
            return Ok(None);
        }
        if is_older(transaction, owner) {
            self.release_transaction(owner);
            return Ok(Some(owner));
        }
        Err(if is_write {
            WoundWaitConflictKind::WriteOwnedByOther
        } else {
            WoundWaitConflictKind::ReadOwnedByOther
        })
    }
}
```

- [ ] **Step 6: Add Wound-Wait test helpers and trait impl**

Add test helpers matching the `NoWaitAbort` helper style:

```rust
#[cfg(test)]
impl WoundWait {
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
    ) -> core::result::Result<(), WoundWaitConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    pub(crate) fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), WoundWaitConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    pub(crate) fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    pub(crate) fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}
```

Add `impl TransactionConcurrencyControl for WoundWait` by delegating to the inherent methods and by collecting `read_versions` keys for `read_granules_for_transaction`.

- [ ] **Step 7: Wire Wound-Wait into state and test imports**

Add `WoundWait(WoundWait)` to `ConcurrencyControlState`, add the `for_config`, `policy`, `policy_mut`, and `remove_owner_for_granule_for_test` matches, and import `WoundWait` plus `WoundWaitConflictKindForTest` in `mod.rs` under `#[cfg(test)]`.

- [ ] **Step 8: Add selected-policy test**

Add:

```rust
#[test]
#[cfg(feature = "transaction-cc-wound-wait")]
fn selected_concurrency_control_uses_wound_wait_semantics() {
    let mut policy = ConcurrencyControlState::for_config(ConcurrencyControl::WoundWait);
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_granule_write(younger, granule, 0).unwrap();
    let wounded = policy.acquire_granule_write(older, granule, 0).unwrap();

    assert_eq!(wounded, Some(younger));
    assert_eq!(policy.owner_for_granule(granule), Some(older));
}
```

- [ ] **Step 9: Run default and feature tests**

Run:

```bash
cargo test -p wasmtime wound_wait_ --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-wound-wait" \
  --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 10: Commit Wound-Wait**

Run:

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
        crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add Wound-Wait transaction concurrency control"
```

---

### Task 4: Add Structured Conflict Actions

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write a failing conflict-action test**

Add:

```rust
#[test]
fn transaction_conflict_action_reports_abort_and_wait_targets() {
    let other = TransactionId::from_raw(2);

    assert_eq!(
        super::concurrency::TransactionConflictAction::Continue.aborted_transaction(),
        None
    );
    assert_eq!(
        super::concurrency::TransactionConflictAction::AbortOther(other)
            .aborted_transaction(),
        Some(other)
    );
    assert_eq!(
        super::concurrency::TransactionConflictAction::WouldWait(other)
            .aborted_transaction(),
        None
    );
    assert_eq!(
        super::concurrency::TransactionConflictAction::WouldWait(other)
            .would_wait_owner(),
        Some(other)
    );
}
```

- [ ] **Step 2: Run the conflict-action test to verify it fails**

Run:

```bash
cargo test -p wasmtime transaction_conflict_action_reports_abort_and_wait_targets --lib -- --format terse
```

Expected: compile failure because `TransactionConflictAction` does not exist.

- [ ] **Step 3: Add the action enum**

In `concurrency.rs`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransactionConflictAction {
    Continue,
    AbortOther(TransactionId),
    WouldWait(TransactionId),
}

impl TransactionConflictAction {
    pub(crate) fn aborted_transaction(self) -> Option<TransactionId> {
        match self {
            Self::AbortOther(transaction) => Some(transaction),
            Self::Continue | Self::WouldWait(_) => None,
        }
    }

    pub(crate) fn would_wait_owner(self) -> Option<TransactionId> {
        match self {
            Self::WouldWait(owner) => Some(owner),
            Self::Continue | Self::AbortOther(_) => None,
        }
    }
}
```

- [ ] **Step 4: Change the trait acquire return type**

Update `TransactionConcurrencyControl`:

```rust
fn acquire_granule_read(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<TransactionConflictAction>;

fn acquire_granule_write(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<TransactionConflictAction>;
```

Update `LockBased`, `NoWaitAbort`, and `WoundWait` trait impls so `None` becomes `TransactionConflictAction::Continue` and `Some(transaction)` becomes `TransactionConflictAction::AbortOther(transaction)`.

- [ ] **Step 5: Preserve existing public acquire behavior in `ConcurrencyControlState`**

Update `ConcurrencyControlState::acquire_granule_read` and `acquire_granule_write` to return `Result<TransactionConflictAction>`.

- [ ] **Step 6: Handle actions in store-local transaction state**

In `state.rs`, import `TransactionConflictAction` through the transaction module path and add:

```rust
fn handle_conflict_action(&mut self, action: TransactionConflictAction) -> Result<()> {
    if let Some(owner) = action.would_wait_owner() {
        bail!(
            "transaction conflict would wait for transaction {}",
            owner.as_raw()
        );
    }
    self.discard_conflict_aborted_transaction(action.aborted_transaction())
}
```

Change `acquire_granule_read_authority` and `acquire_granule_write_authority` to return `Result<TransactionConflictAction>`. In `acquire_granule_read` and `acquire_granule_write`, replace `if aborted.is_some()` with:

```rust
let refresh_after_abort = action.aborted_transaction().is_some();
self.handle_conflict_action(action)?;
if refresh_after_abort {
    let current_version = self.current_version_for_granule(granule, current_version)?;
    self.refresh_read_version_authority(transaction, granule, current_version)?;
}
```

- [ ] **Step 7: Handle actions in shared runtime**

In `region_runtime.rs`, change shared acquire methods to return `Result<TransactionConflictAction>`. After policy acquire:

```rust
if let Some(aborted) = action.aborted_transaction() {
    runtime.conflict_aborted_transactions.insert(aborted);
}
Ok(action)
```

Do not insert anything into `conflict_aborted_transactions` for `WouldWait`.

- [ ] **Step 8: Update tests that expected `Option<TransactionId>`**

For tests that still need the old shape, compare `action.aborted_transaction()` instead of the raw return value:

```rust
let action = policy.acquire_granule_write(older, granule, 0).unwrap();
assert_eq!(action.aborted_transaction(), Some(younger));
```

- [ ] **Step 9: Run regression tests**

Run:

```bash
cargo test -p wasmtime transaction_conflict_action_reports_abort_and_wait_targets --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 10: Commit structured conflict actions**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add structured transaction conflict actions"
```

---

### Task 5: Add Wait-Die Without Blocking

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing Wait-Die direct tests**

Add:

```rust
#[test]
fn wait_die_older_requester_would_wait_for_younger_owner() {
    let mut policy = WaitDie::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(younger, granule, 3).unwrap();
    let action = policy.acquire_write(older, granule, 3).unwrap();

    assert_eq!(
        action,
        super::concurrency::TransactionConflictAction::WouldWait(younger)
    );
    assert_eq!(policy.owner_for_test(granule), Some(younger));
}

#[test]
fn wait_die_younger_requester_dies_against_older_owner() {
    let mut policy = WaitDie::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(older, granule, 3).unwrap();

    let error = policy
        .acquire_write_result_for_test(younger, granule, 3)
        .unwrap_err();
    assert_eq!(error, WaitDieConflictKindForTest::WriteOwnedByOther);
    assert_eq!(policy.owner_for_test(granule), Some(older));
}
```

- [ ] **Step 2: Run Wait-Die tests to verify they fail**

Run:

```bash
cargo test -p wasmtime wait_die_ --lib -- --format terse
```

Expected: compile failure because `WaitDie` does not exist.

- [ ] **Step 3: Add feature flags and config wiring**

Add `transaction-cc-wait-die` to both Cargo manifests. Add `WaitDie` to `ConcurrencyControl`, `default_for_build`, and `ConcurrencyControlState`.

- [ ] **Step 4: Implement `WaitDie`**

Use the same state as `WoundWait`:

```rust
#[derive(Debug, Default)]
pub(crate) struct WaitDie {
    pub(crate) owners: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}
```

The conflict rule is:

```rust
if is_older(transaction, owner) {
    return Ok(TransactionConflictAction::WouldWait(owner));
}
Err(if is_write {
    WaitDieConflictKind::WriteOwnedByOther
} else {
    WaitDieConflictKind::ReadOwnedByOther
})
```

Reads and writes should otherwise match the `NoWaitAbort` read-version behavior. `WaitDie::acquire_write` and `WaitDie::record_read` should return `Result<TransactionConflictAction>`.

- [ ] **Step 5: Add Wait-Die selected-policy test**

Add:

```rust
#[test]
#[cfg(feature = "transaction-cc-wait-die")]
fn selected_concurrency_control_uses_wait_die_would_wait_semantics() {
    let mut policy = ConcurrencyControlState::for_config(ConcurrencyControl::WaitDie);
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_granule_write(younger, granule, 0).unwrap();
    let action = policy.acquire_granule_write(older, granule, 0).unwrap();

    assert_eq!(
        action,
        super::concurrency::TransactionConflictAction::WouldWait(younger)
    );
    assert_eq!(policy.owner_for_granule(granule), Some(younger));
}
```

- [ ] **Step 6: Run default and feature tests**

Run:

```bash
cargo test -p wasmtime wait_die_ --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-wait-die" \
  --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 7: Commit Wait-Die**

Run:

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
        crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add nonblocking Wait-Die transaction concurrency control"
```

---

### Task 6: Add Strict Two-Phase Locking

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing Strict 2PL tests**

Add:

```rust
#[test]
fn strict_2pl_allows_multiple_readers_and_blocks_writer() {
    let mut policy = StrictTwoPhaseLocking::default();
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let writer = TransactionId::from_raw(3);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.record_read_for_test(first, granule, 5).unwrap();
    policy.record_read_for_test(second, granule, 5).unwrap();

    let error = policy
        .acquire_write_result_for_test(writer, granule, 5)
        .unwrap_err();
    assert_eq!(
        error,
        StrictTwoPhaseLockingConflictKindForTest::WriteReadLockedByOther
    );
}

#[test]
fn strict_2pl_upgrades_only_for_sole_reader() {
    let mut policy = StrictTwoPhaseLocking::default();
    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.record_read_for_test(transaction, granule, 5).unwrap();
    policy.acquire_write_for_test(transaction, granule, 5).unwrap();

    assert_eq!(policy.owner_for_test(granule), Some(transaction));
}
```

- [ ] **Step 2: Run Strict 2PL tests to verify they fail**

Run:

```bash
cargo test -p wasmtime strict_2pl_ --lib -- --format terse
```

Expected: compile failure because `StrictTwoPhaseLocking` does not exist.

- [ ] **Step 3: Add feature flags and config wiring**

Add `transaction-cc-strict-2pl` to both Cargo manifests. Add `StrictTwoPhaseLocking` to `ConcurrencyControl`, `default_for_build`, and `ConcurrencyControlState`.

- [ ] **Step 4: Implement Strict 2PL state**

In `concurrency.rs`, import `BTreeSet` if not already available and add:

```rust
#[derive(Debug, Default)]
pub(crate) struct StrictTwoPhaseLocking {
    pub(crate) readers: BTreeMap<GranuleId, BTreeSet<TransactionId>>,
    pub(crate) writers: BTreeMap<GranuleId, TransactionId>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}
```

Add `StrictTwoPhaseLockingConflictKind` with variants:

```rust
ReadOwnedByOther,
ReadVersionMismatch,
WriteOwnedByOther,
WriteReadLockedByOther,
WriteVersionMismatch,
```

- [ ] **Step 5: Implement Strict 2PL rules**

Use these rules in the inherent methods:

```rust
fn has_other_reader(&self, transaction: TransactionId, granule: GranuleId) -> bool {
    self.readers
        .get(&granule)
        .is_some_and(|readers| readers.iter().any(|reader| *reader != transaction))
}

fn record_read_typed(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    version: u64,
) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKind> {
    if self
        .writers
        .get(&granule)
        .is_some_and(|writer| *writer != transaction)
    {
        return Err(StrictTwoPhaseLockingConflictKind::ReadOwnedByOther);
    }
    self.readers.entry(granule).or_default().insert(transaction);
    self.read_versions.entry((transaction, granule)).or_insert(version);
    Ok(TransactionConflictAction::Continue)
}

fn acquire_write_typed(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> core::result::Result<TransactionConflictAction, StrictTwoPhaseLockingConflictKind> {
    if self
        .writers
        .get(&granule)
        .is_some_and(|writer| *writer != transaction)
    {
        return Err(StrictTwoPhaseLockingConflictKind::WriteOwnedByOther);
    }
    if self.has_other_reader(transaction, granule) {
        return Err(StrictTwoPhaseLockingConflictKind::WriteReadLockedByOther);
    }
    if self
        .read_versions
        .get(&(transaction, granule))
        .is_some_and(|version| *version != current_version)
    {
        return Err(StrictTwoPhaseLockingConflictKind::WriteVersionMismatch);
    }
    self.writers.insert(granule, transaction);
    self.readers.entry(granule).or_default().insert(transaction);
    self.read_versions
        .entry((transaction, granule))
        .or_insert(current_version);
    Ok(TransactionConflictAction::Continue)
}
```

Release must remove the transaction from `writers`, every reader set, and `read_versions`.

- [ ] **Step 6: Implement trait and test helpers**

Implement `TransactionConcurrencyControl` for `StrictTwoPhaseLocking`. `owner_for_granule` returns `writers.get(&granule).copied()`. `read_granules_for_transaction` collects `read_versions` keys.

Add test helpers matching previous policy helper names: `record_read_for_test`, `acquire_write_for_test`, `record_read_result_for_test`, `acquire_write_result_for_test`, `abort_for_test`, `owner_for_test`.

- [ ] **Step 7: Run default and feature tests**

Run:

```bash
cargo test -p wasmtime strict_2pl_ --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-strict-2pl" \
  --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 8: Commit Strict 2PL**

Run:

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
        crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add strict 2PL transaction concurrency control"
```

---

### Task 7: Add Commit-Time Write Validation Hooks

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing generic trait-contract assertions**

Extend `transaction_concurrency_control_trait_covers_runtime_policy_contract` so the generic `T: TransactionConcurrencyControl` block calls:

```rust
policy
    .validate_write(result_release_transaction, result_release_write_granule, 1)
    .unwrap();
assert!(policy
    .write_granules_for_transaction(result_release_transaction)
    .contains(&result_release_write_granule));
policy
    .commit_transaction_result(result_release_transaction)
    .unwrap();
```

Expected behavior for existing owner-based policies: write validation succeeds while the transaction owns the write granule, write granule enumeration includes the owned granule, and commit-result hook succeeds without changing semantics.

- [ ] **Step 2: Run the trait-contract test to verify it fails**

Run:

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract --lib -- --format terse
```

Expected: compile failure because the trait methods do not exist.

- [ ] **Step 3: Extend `TransactionConcurrencyControl`**

Add default-capable methods:

```rust
fn validate_write(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<()>;

fn write_granules_for_transaction(&self, transaction: TransactionId) -> Vec<GranuleId>;

fn commit_transaction_result(&mut self, _transaction: TransactionId) -> Result<()> {
    Ok(())
}
```

Owner-based policies should implement `validate_write` by checking that `owner_for_granule(granule) == Some(transaction)` or by accepting no owner only when the policy never owns writes. `write_granules_for_transaction` should enumerate owner maps for owner-based policies.

- [ ] **Step 4: Add state and shared-runtime forwarding**

In `ConcurrencyControlState`, add forwarding methods for `validate_write`, `write_granules_for_transaction`, and `commit_transaction_result`.

In `region_runtime.rs`, add:

```rust
pub(crate) fn validate_granule_write(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<()> {
    self.lock_authority()?
        .concurrency
        .validate_write(transaction, granule, current_version)
}

pub(crate) fn commit_transaction_result(&self, transaction: TransactionId) -> Result<()> {
    self.lock_authority()?
        .concurrency
        .commit_transaction_result(transaction)
}
```

In `state.rs`, add `validate_granule_write_authority`, `commit_transaction_authority`, `active_write_granules`, and `validate_active_writes_with`. Use `self.write_granules` as the authoritative active write set for both local and shared runtime transactions.

- [ ] **Step 5: Validate writes before commit publication**

Change read validation helpers so the same current-version closure can be used for read and write validation:

```rust
pub(crate) fn validate_active_reads_with<F>(&self, mut current_version_fn: F) -> Result<()>
where
    F: FnMut(GranuleId) -> Result<u64>,
{
    self.validate_active_read_granules_with(&mut current_version_fn)
}

fn validate_active_read_granules_with<F>(&self, current_version_fn: &mut F) -> Result<()>
where
    F: FnMut(GranuleId) -> Result<u64>,
{
    for granule in self.active_read_granules()? {
        let current_version = current_version_fn(granule)?;
        self.validate_active_read(granule, current_version)?;
    }
    Ok(())
}

fn validate_active_writes_with<F>(&self, current_version_fn: &mut F) -> Result<()>
where
    F: FnMut(GranuleId) -> Result<u64>,
{
    let transaction = self.active_transaction_required()?;
    for granule in self.write_granules.iter().copied() {
        let current_version = current_version_fn(granule)?;
        self.validate_granule_write_authority(transaction, granule, current_version)?;
    }
    Ok(())
}
```

In `commit_with_read_validation`, call read validation and write validation before `staged_records()`:

```rust
let mut current_version_fn = current_version_fn;
self.validate_active_read_granules_with(&mut current_version_fn)?;
self.validate_active_writes_with(&mut current_version_fn)?;
```

- [ ] **Step 6: Run regression tests**

Run:

```bash
cargo test -p wasmtime transaction_concurrency_control_trait_covers_runtime_policy_contract --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass and default policy behavior remains unchanged.

- [ ] **Step 7: Commit write-validation hooks**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add transaction write-validation policy hooks"
```

---

### Task 8: Add Optimistic Validation Only

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing optimistic-validation tests**

Add:

```rust
#[test]
fn optimistic_validation_records_writes_without_ownership() {
    let mut policy = OptimisticValidation::default();
    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write(transaction, granule, 5).unwrap();

    assert_eq!(policy.owner_for_granule(granule), None);
    assert_eq!(
        policy.write_granules_for_transaction(transaction),
        vec![granule]
    );
    policy.validate_write(transaction, granule, 5).unwrap();
}

#[test]
fn optimistic_validation_rejects_changed_write_version() {
    let mut policy = OptimisticValidation::default();
    let transaction = TransactionId::from_raw(1);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write(transaction, granule, 5).unwrap();

    let error = policy
        .validate_write_result_for_test(transaction, granule, 6)
        .unwrap_err();
    assert_eq!(
        error,
        OptimisticValidationConflictKindForTest::WriteVersionMismatch
    );
}
```

- [ ] **Step 2: Run optimistic-validation tests to verify they fail**

Run:

```bash
cargo test -p wasmtime optimistic_validation_ --lib -- --format terse
```

Expected: compile failure because `OptimisticValidation` does not exist.

- [ ] **Step 3: Add feature flags and config wiring**

Add `transaction-cc-optimistic-validation` to both Cargo manifests. Add `OptimisticValidation` to `ConcurrencyControl`, `default_for_build`, and `ConcurrencyControlState`.

- [ ] **Step 4: Implement Optimistic Validation state**

Add:

```rust
#[derive(Debug, Default)]
pub(crate) struct OptimisticValidation {
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}
```

Conflict kind variants:

```rust
ReadVersionMismatch,
WriteVersionMismatch,
```

Rules:

- `record_read` records or validates the read version.
- `acquire_write` records or validates the write version and returns `TransactionConflictAction::Continue`.
- `owner_for_granule` always returns `None`.
- `validate_write` checks `write_versions[(transaction, granule)] == current_version`.
- release removes both read and write version entries for the transaction.

- [ ] **Step 5: Add selected-policy test**

Add:

```rust
#[test]
#[cfg(feature = "transaction-cc-optimistic-validation")]
fn selected_concurrency_control_uses_optimistic_validation_semantics() {
    let mut policy = ConcurrencyControlState::for_config(ConcurrencyControl::OptimisticValidation);
    let first = TransactionId::from_raw(1);
    let second = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_granule_write(first, granule, 5).unwrap();
    policy.acquire_granule_write(second, granule, 5).unwrap();

    assert_eq!(policy.owner_for_granule(granule), None);
    policy.validate_write(first, granule, 5).unwrap();
    policy.validate_write(second, granule, 5).unwrap();
}
```

- [ ] **Step 6: Run default and feature tests**

Run:

```bash
cargo test -p wasmtime optimistic_validation_ --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-optimistic-validation" \
  --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 7: Commit Optimistic Validation**

Run:

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
        crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add optimistic-validation transaction concurrency control"
```

---

### Task 9: Add Commit-Success Metadata Hook

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add commit-success call in completion paths**

In `complete_commit`, call `commit_transaction_authority(transaction)` after `bump_active_versioned_write_granules()` and before `clear_active()`:

```rust
let transaction = self.active_transaction_required()?;
self.begin_terminal_commit()?;
self.ensure_no_pending_conflict_aborted_allocated_objects()?;
self.retry_post_commit_linear_undo_retirement();
self.bump_active_versioned_write_granules()?;
self.commit_transaction_authority(transaction)?;
self.clear_active()?;
```

Apply the same ordering in `complete_commit_with_persistent_root_delta` for both local and shared paths.

- [ ] **Step 2: Add a no-op regression test**

Add a test that commits a default-policy transaction and verifies existing behavior still passes:

```rust
#[test]
fn commit_success_policy_hook_preserves_default_commit_behavior() {
    let mut state = TransactionState::default();
    let transaction = state.begin().unwrap();

    state.stage_global(0, GlobalSnapshot::I32(7)).unwrap();
    state.complete_commit().unwrap();

    assert_eq!(state.active_transaction(), None);
    assert_ne!(transaction.as_raw(), 0);
}
```

- [ ] **Step 3: Run regression tests and commit**

Run:

```bash
cargo test -p wasmtime commit_success_policy_hook_preserves_default_commit_behavior --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
git add crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Call transaction policy commit-success hook"
```

Expected: all commands pass.

---

### Task 10: Add Pure Timestamp Ordering

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing timestamp-ordering tests**

Add:

```rust
#[test]
fn timestamp_ordering_rejects_write_older_than_read_timestamp() {
    let mut policy = TimestampOrdering::default();
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.record_read_for_test(younger, granule, 5).unwrap();

    let error = policy
        .acquire_write_result_for_test(older, granule, 5)
        .unwrap_err();
    assert_eq!(
        error,
        TimestampOrderingConflictKindForTest::WriteReadTimestampTooNew
    );
}

#[test]
fn timestamp_ordering_committed_write_advances_write_timestamp() {
    let mut policy = TimestampOrdering::default();
    let transaction = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(transaction, granule, 5).unwrap();
    policy.commit_transaction_result(transaction).unwrap();

    assert_eq!(
        policy.write_timestamp_for_test(granule),
        Some(transaction)
    );
}

#[test]
fn timestamp_ordering_aborted_write_does_not_advance_write_timestamp() {
    let mut policy = TimestampOrdering::default();
    let transaction = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_write_for_test(transaction, granule, 5).unwrap();
    policy.release_transaction(transaction);

    assert_eq!(policy.write_timestamp_for_test(granule), None);
}
```

- [ ] **Step 2: Run timestamp-ordering tests to verify they fail**

Run:

```bash
cargo test -p wasmtime timestamp_ordering_ --lib -- --format terse
```

Expected: compile failure because `TimestampOrdering` does not exist.

- [ ] **Step 3: Add feature flags and config wiring**

Add `transaction-cc-timestamp-ordering` to both Cargo manifests. Add `TimestampOrdering` to `ConcurrencyControl`, `default_for_build`, and `ConcurrencyControlState`.

- [ ] **Step 4: Implement Timestamp Ordering state**

Add:

```rust
#[derive(Debug, Default)]
pub(crate) struct TimestampOrdering {
    pub(crate) read_timestamps: BTreeMap<GranuleId, TransactionTimestamp>,
    pub(crate) write_timestamps: BTreeMap<GranuleId, TransactionTimestamp>,
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}
```

Conflict kind variants:

```rust
ReadWriteTimestampTooNew,
ReadVersionMismatch,
WriteReadTimestampTooNew,
WriteWriteTimestampTooNew,
WriteVersionMismatch,
```

- [ ] **Step 5: Implement Timestamp Ordering rules**

Read rule:

```rust
if self
    .write_timestamps
    .get(&granule)
    .is_some_and(|write_timestamp| timestamp_for_transaction(transaction) < *write_timestamp)
{
    return Err(TimestampOrderingConflictKind::ReadWriteTimestampTooNew);
}
self.read_timestamps
    .entry(granule)
    .and_modify(|timestamp| *timestamp = (*timestamp).max(timestamp_for_transaction(transaction)))
    .or_insert(timestamp_for_transaction(transaction));
```

Write rule:

```rust
let transaction_timestamp = timestamp_for_transaction(transaction);
if self
    .read_timestamps
    .get(&granule)
    .is_some_and(|read_timestamp| transaction_timestamp < *read_timestamp)
{
    return Err(TimestampOrderingConflictKind::WriteReadTimestampTooNew);
}
if self
    .write_timestamps
    .get(&granule)
    .is_some_and(|write_timestamp| transaction_timestamp < *write_timestamp)
{
    return Err(TimestampOrderingConflictKind::WriteWriteTimestampTooNew);
}
self.write_versions
    .entry((transaction, granule))
    .or_insert(current_version);
```

Commit hook:

```rust
fn commit_transaction_result(&mut self, transaction: TransactionId) -> Result<()> {
    let committed = self.write_versions
        .keys()
        .filter_map(|(writer, granule)| (*writer == transaction).then_some(*granule))
        .collect::<Vec<_>>();
    for granule in committed {
        self.write_timestamps
            .entry(granule)
            .and_modify(|timestamp| {
                *timestamp = (*timestamp).max(timestamp_for_transaction(transaction));
            })
            .or_insert(timestamp_for_transaction(transaction));
    }
    Ok(())
}
```

Release removes transaction-local read/write version entries, but must not remove committed `read_timestamps` or `write_timestamps`.

- [ ] **Step 6: Add selected-policy test**

Add:

```rust
#[test]
#[cfg(feature = "transaction-cc-timestamp-ordering")]
fn selected_concurrency_control_uses_timestamp_ordering_semantics() {
    let mut policy = ConcurrencyControlState::for_config(ConcurrencyControl::TimestampOrdering);
    let older = TransactionId::from_raw(1);
    let younger = TransactionId::from_raw(2);
    let granule = GranuleId::TMemory {
        instance: Some(1),
        memory_index: 0,
        granule_index: 7,
    };

    policy.acquire_granule_read(younger, granule, 5).unwrap();
    let err = policy.acquire_granule_write(older, granule, 5).unwrap_err();

    assert!(
        err.to_string().contains("timestamp"),
        "{err:?}"
    );
}
```

- [ ] **Step 7: Run default and feature tests**

Run:

```bash
cargo test -p wasmtime timestamp_ordering_ --lib -- --format terse
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-timestamp-ordering" \
  --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [ ] **Step 8: Commit Timestamp Ordering**

Run:

```bash
git add Cargo.toml crates/wasmtime/Cargo.toml \
        crates/wasmtime/src/runtime/transaction/config.rs \
        crates/wasmtime/src/runtime/transaction/concurrency.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add timestamp-ordering transaction concurrency control"
```

---

### Task 11: Final Matrix Verification And Roadmap Cleanup

**Files:**
- Modify: `docs/shisoft/2026-06-19-five-concurrency-controls-implementation-roadmap.md`
- Modify: `docs/shisoft/2026-06-19-five-concurrency-controls-implementation-plan.md`

- [x] **Step 1: Run the default verification matrix**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

- [x] **Step 2: Run every alternate policy feature**

Run one command per feature:

```bash
for feature in \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering
do
  cargo test -p wasmtime --no-default-features \
    --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,${feature}" \
    --lib transaction:: -- --format terse
done
```

Expected: every feature-specific test run passes.

- [x] **Step 3: Update roadmap status**

In `docs/shisoft/2026-06-19-five-concurrency-controls-implementation-roadmap.md`, append:

```markdown
## Implementation Status

- Wound-Wait: implemented
- Wait-Die: implemented without blocking; `WouldWait` is a structured conflict action
- Strict Two-Phase Locking: implemented
- Optimistic Validation Only: implemented with commit-time write validation
- Pure Timestamp Ordering: implemented as single-version timestamp ordering
- MVCC: still deferred to a separate storage, recovery, and garbage-collection roadmap
```

- [x] **Step 4: Commit final docs update**

Run:

```bash
git add docs/shisoft/2026-06-19-five-concurrency-controls-implementation-roadmap.md \
        docs/shisoft/2026-06-19-five-concurrency-controls-implementation-plan.md
git commit -m "Document five concurrency-control implementation status"
```

Expected: final docs commit is created after all verification passes.

## Final Verification Results

Default verification:

- `cargo test -p wasmtime --lib transaction:: -- --format terse`: 492 passed
- `cargo fmt --all -- --check`: passed
- `git diff --check`: passed

Alternate policy feature verification:

- `transaction-cc-nowait-abort`: 488 passed
- `transaction-cc-wound-wait`: 484 passed
- `transaction-cc-wait-die`: 485 passed
- `transaction-cc-strict-2pl`: 484 passed
- `transaction-cc-optimistic-validation`: 484 passed
- `transaction-cc-timestamp-ordering`: 484 passed
- `transaction-cc-lockbased`: 492 passed

Implementation status:

- Wound-Wait: implemented
- Wait-Die: implemented without blocking; `WouldWait` is a structured conflict action
- Strict Two-Phase Locking: implemented
- Optimistic Validation Only: implemented with commit-time write validation
- Pure Timestamp Ordering: implemented as single-version timestamp ordering
- MVCC: still deferred to a separate storage, recovery, and garbage-collection roadmap
