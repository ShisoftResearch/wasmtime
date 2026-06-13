# Transactional Wasm Model Checking Test Roadmap

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add model-checking-style transaction tests that verify atomicity, recovery, isolation, durable publication, and backend behavior beyond example-based WAST tests.

**Architecture:** Start with deterministic Rust reference models and `proptest` generators over small transaction histories. Compare the model against the real `TxDurableLog`, file-backed recovery scan, `TMemory` undo application, `ObjectTable` rebuild helpers, and `LockBased` ownership state. Keep model tests in unit-test modules near the runtime code they verify, and reserve true thread interleaving tools such as `loom` for the later shared-concurrency implementation.

**Tech Stack:** Rust unit tests, `proptest`, `tempfile`, Wasmtime transaction runtime internals, file-backed block-region storage, transaction proposal WAST harness.

---

## Scope

This roadmap covers test work only. It should not change transaction semantics except where a test exposes a real correctness bug and a follow-up implementation fix is required.

The model tests should cover:

- durable log/recovery semantics
- file-backed `TMemory` undo recovery
- `TObjectPub` redo recovery and `ObjectTable` rebuild
- mixed tmemory plus object transactions with one LP
- lock-based conflict and ownership rules
- transaction boundary invariants
- permission-state invariants
- malformed/corrupt log detection
- backend conformance for future PMEM/file implementations

Out of scope for this roadmap:

- persistent-object GC
- real PMEM hardware validation
- `loom`-based thread scheduling before lock ownership is backed by shared thread-safe runtime state
- changing proposal WAST files

## Correctness Invariants

All model tests should map back to these invariants.

1. **Atomic commit:** A transaction with LP is fully committed for all roles in that transaction.
2. **Crash abort:** A transaction without LP is treated as aborted during recovery.
3. **Undo-vs-redo roles:** `TMemoryUndo` records roll back loose-end in-place linear-memory writes. `TObjectPub` records publish committed object versions only when LP exists.
4. **One LP per logical transaction:** Mixed object and tmemory records share one final LP marker.
5. **Highest version wins:** For each logical object/granule publication, the highest committed version wins.
6. **Duplicate committed versions are corruption:** Same logical id and version with different data pointers or payloads must not recover silently.
7. **Committed undo is obsolete:** `TMemoryUndo` entries in LP-marked transactions must not produce rollback actions.
8. **Loose-end object publications are invisible:** `TObjectPub` entries without LP must not rebuild an object winner.
9. **Ownership isolation:** At most one transaction owns a writable granule at a time.
10. **Abort releases ownership:** Abort, trap, conflict abort, and loose-end cleanup release owned granules.
11. **Permission state is transactional state:** Permission transitions apply to transaction/granule access state, not to the persistent reference value itself.
12. **Backend abstraction:** File-backed storage is one `TxDurableLogBackend` implementation; future PMEM backends must satisfy the same model tests.

## File Map

- `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Add durable-log reference models and property tests.
  - Test `StreamPublisher`, `TxDurableLog`, LP handling, role separation, and file-backed recovery summaries.
- `crates/wasmtime/src/runtime/transaction.rs`
  - Add transaction-state, mixed participant, object table, permission, and `LockBased` state-space tests.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Add focused recovery scan model tests for malformed logs, version selection, and loose-end grouping if the test needs direct scanner access.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add backend image/corruption tests only when raw block mutation is required.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record each completed model-test wave, command output, and any bug it finds.
- `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Update only if the tests expose an ambiguity in the intended transaction semantics.

## Test Module Layout

Use nested test modules so model tests remain discoverable and separately runnable:

```rust
#[cfg(test)]
mod tests {
    mod model_recovery {
        // TxDurableLog and recovery model tests.
    }

    mod model_mixed_participants {
        // TMemory plus TObjectPub model tests.
    }

    mod model_lock_based {
        // Deterministic LockBased state-space tests.
    }

    mod model_permissions {
        // Transaction permission-state tests.
    }
}
```

Run model tests with narrow filters first:

```bash
cargo test -p wasmtime --lib model_recovery -- --format terse
cargo test -p wasmtime --lib model_mixed_participants -- --format terse
cargo test -p wasmtime --lib model_lock_based -- --format terse
cargo test -p wasmtime --lib model_permissions -- --format terse
```

Then run the existing broader transaction gates:

```bash
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib block_region -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo fmt --check
git diff --check
```

## Wave 1: Durable Log Recovery Reference Model

**Purpose:** Verify transaction log recovery against a small independent model.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add model types under `model_recovery`**

Add test-only model types:

```rust
#[derive(Clone, Debug)]
enum ModelRole {
    TObjectPub,
    TMemoryUndo,
}

#[derive(Clone, Debug)]
struct ModelRecord {
    tx: u32,
    logical_id: u64,
    version: u32,
    role: ModelRole,
    payload_byte: u8,
    has_lp: bool,
}
```

- [ ] **Step 2: Write the first failing deterministic model test**

Test name:

```rust
#[test]
fn model_recovery_committed_object_pub_wins_and_committed_undo_is_ignored()
```

Scenario:

- transaction 1 appends one `TMemoryUndo`
- transaction 1 appends one `TObjectPub`
- transaction 1 publishes LP
- recovery returns one object winner
- recovery returns zero tmemory rollbacks

Run:

```bash
cargo test -p wasmtime --lib model_recovery_committed_object_pub_wins_and_committed_undo_is_ignored -- --format terse
```

Expected before implementation if helpers are missing: FAIL with missing model/helper code. Expected after implementation: PASS.

- [ ] **Step 3: Add a reference-model evaluator**

Add a pure function in the test module:

```rust
fn expected_recovery(records: &[ModelRecord]) -> ExpectedRecovery
```

It must:

- group records by `tx`
- treat `has_lp` as commit
- collect `TObjectPub` winners only from committed transactions
- collect `TMemoryUndo` rollbacks only from uncommitted transactions
- choose highest version per logical id for object winners

- [ ] **Step 4: Add `proptest` generation for small histories**

Generate:

- 1 to 4 transactions
- 1 to 8 records total
- 1 to 3 logical object ids
- 1 to 3 logical tmemory granule ids
- versions in `1..4`
- committed vs loose-end transaction flag

Keep generated payloads one byte repeated into the real encoded payload to make failures easy to read.

- [ ] **Step 5: Compare model recovery to real file-backed recovery**

For each generated history:

- create a temp file-backed `TxDurableLog`
- publish generated records through `StreamPublisher`
- publish LP for committed model transactions only
- reopen with `TxDurableLog::recover_file_backed_for_test`
- compare recovered object winners and rollback counts against `expected_recovery`

- [ ] **Step 6: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_recovery -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add durable transaction recovery model tests"
```

## Wave 2: Crash Cutpoint Model Tests

**Purpose:** Verify the Zen-style ordering rule at visible recovery cutpoints.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs` if direct scanner tests are cleaner
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define cutpoints**

Use this enum in test code:

```rust
#[derive(Clone, Copy, Debug)]
enum CrashCutpoint {
    BeforeAnyRecord,
    AfterUndoRecordBeforeLp,
    AfterObjectRecordBeforeLp,
    AfterAllRecordsBeforeLp,
    AfterLp,
}
```

- [ ] **Step 2: Add deterministic tests for each cutpoint**

Test names:

```rust
#[test]
fn model_crash_before_lp_rolls_back_tmemory_and_drops_objects()

#[test]
fn model_crash_after_lp_commits_objects_and_ignores_undo()
```

- [ ] **Step 3: Add generated cutpoint tests**

Generate a committed logical transaction and truncate publication at each cutpoint by choosing which publish calls happen.

Expected:

- no LP means tmemory rollback actions and no object winners
- LP means object winners and no tmemory rollback actions

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_crash -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transaction crash cutpoint model tests"
```

## Wave 3: Mixed File-Backed TMemory And TObject Model

**Purpose:** Verify the real participant behavior, not only log summaries.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define a small abstract state**

Use test-only state:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelWorld {
    tmemory_granules: std::collections::BTreeMap<u32, u8>,
    object_fields: std::collections::BTreeMap<u64, i32>,
}
```

- [ ] **Step 2: Add deterministic mixed commit test**

Test name:

```rust
#[test]
fn model_mixed_file_backed_commit_matches_reference_world()
```

Scenario:

- start with tmemory granule 0 byte `0x11`
- start with persistent object field `1`
- transaction writes granule 0 to `0x22`
- transaction writes object field to `9`
- publish LP
- recover
- assert file-backed tmemory image starts with `0x22`
- rebuild object table and assert object field is `9`

- [ ] **Step 3: Add deterministic mixed loose-end test**

Test name:

```rust
#[test]
fn model_mixed_file_backed_loose_end_matches_reference_world()
```

Scenario:

- start with tmemory granule 0 byte `0x33`
- start with persistent object field `1`
- publish undo and object publication
- do not publish LP
- recover
- apply rollback
- assert file-backed tmemory image starts with `0x33`
- assert no object winner rebuilds

- [ ] **Step 4: Add generated one-transaction mixed tests**

Generate:

- 1 to 3 tmemory granule writes
- 0 to 2 persistent object writes
- commit flag

Model expected state:

- if committed, tmemory has new bytes and objects have new fields
- if loose-end, tmemory has old bytes and objects have old fields or no new winners

- [ ] **Step 5: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_mixed_participants -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add mixed participant transaction model tests"
```

## Wave 4: Version And Corruption Model Tests

**Purpose:** Lock down recovery selection and corruption behavior.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Highest-version deterministic tests**

Add tests:

```rust
#[test]
fn model_recovery_selects_highest_committed_object_version()

#[test]
fn model_recovery_delete_version_beats_older_live_version_when_delete_exists()
```

If delete entries are not wired yet, omit the delete test from execution and add it only when delete semantics are implemented.

- [ ] **Step 2: Duplicate-version corruption tests**

Add tests:

```rust
#[test]
fn model_recovery_rejects_distinct_duplicate_committed_object_version()

#[test]
fn model_recovery_accepts_idempotent_lp_duplicate_pointer()
```

- [ ] **Step 3: CRC and malformed-entry tests**

Use block-region raw helpers to corrupt:

- log-entry CRC
- role field
- data pointer outside a data chunk
- data chunk stream id that does not match the role

Each corruption test must assert the exact error category substring that the recovery code returns.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_recovery -- --format terse
cargo test -p wasmtime --lib block_region -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transaction recovery corruption model tests"
```

## Wave 5: LockBased State-Space Tests

**Purpose:** Exhaustively verify bounded lock ownership and conflict behavior without real threads.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define lock-model operations**

Use test-only operations:

```rust
#[derive(Clone, Copy, Debug)]
enum LockModelOp {
    Read { tx: u64, granule: u8, version: u64 },
    Write { tx: u64, granule: u8, version: u64 },
    Abort { tx: u64 },
    Release { tx: u64 },
}
```

- [ ] **Step 2: Add exhaustive small schedule generation**

Enumerate schedules up to length 5 over:

- transactions `1` and `2`
- granules `0` and `1`
- versions `0` and `1`

Do not use OS threads. Apply operations sequentially to `LockBased` and to a small reference model.

- [ ] **Step 3: Assert lock invariants after each operation**

For each operation prefix, assert:

- no granule has more than one owner
- a transaction owns exactly the granules in its owner set
- abort/release removes all ownership for that transaction
- conflicting write does not steal ownership from the existing owner
- read conflict against another writer fails
- optimistic read version mismatch fails

- [ ] **Step 4: Add generated schedule tests with `proptest`**

Generate operation sequences of length `0..20` and compare the same invariants.

- [ ] **Step 5: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_lock_based -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add lock based transaction model tests"
```

## Wave 6: Permission-State Model Tests

**Purpose:** Move permissions from example tests into a small transition model aligned with the meeting design.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md` only if the meeting semantics need clearer wording
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define permission model states**

Use the runtime permission vocabulary already implemented in `transaction.rs`. The test model should represent permissions as transaction/granule access state, not as fields on the reference value.

- [ ] **Step 2: Add deterministic transition tests**

Add tests for:

- read permission allows read and does not grant write
- write permission grants staged mutation
- write ownership implies read of the staged value
- permission downgrade rejects future writes
- permission state is discarded on abort
- permission state is released on commit

- [ ] **Step 3: Add generated permission transition tests**

Generate short sequences:

- grant read
- grant write
- read
- write
- downgrade
- abort
- commit/release

Compare runtime behavior to the reference permission-state machine.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_permissions -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transaction permission model tests"
```

## Wave 7: Transaction Boundary And Active-State Tests

**Purpose:** Verify host/tfunc/tcall active-transaction invariants outside individual helper calls.

**Files:**
- Modify: `tests/wast.rs` only if a new harness filter is needed
- Modify: `crates/test-util/src/wast.rs` only if existing transaction proposal harness cannot express the check
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add unit-level active-state tests**

Add tests for:

- no transactional memory/global/table/object operation succeeds without active transaction
- exported top-level `tfunc` starts and clears transaction state
- trap path aborts and clears transaction state
- nested transactional call reuses active transaction

- [ ] **Step 2: Add WAST-level smoke tests if parser/runtime support exists**

Use real transaction syntax, not normalized fixtures. Add new WAST files only if they are local research tests, not proposal test-case edits.

- [ ] **Step 3: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib transaction_active -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/test-util/src/wast.rs tests docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add transaction boundary model tests"
```

## Wave 8: Backend Conformance Test Harness

**Purpose:** Ensure future PMEM/file backends satisfy the same durable-log behavior.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define a test backend factory trait**

Use a test-only trait:

```rust
trait DurableLogTestFactory {
    fn create(&self) -> TxDurableLog;
    fn recover(&self) -> Option<crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion>;
}
```

- [ ] **Step 2: Add file-backed factory**

The file-backed factory must:

- create a temp path
- create `TxDurableLog::create_file_backed`
- recover with `TxDurableLog::recover_file_backed_for_test`

- [ ] **Step 3: Add in-memory factory for non-restart properties**

The in-memory factory should run append/flush/fence ordering and log-entry tests that do not require reopen recovery.

- [ ] **Step 4: Run the same model scenarios through all compatible factories**

Scenarios:

- object-only commit
- tmemory-undo-only commit
- mixed commit
- mixed loose end
- multi-stream transaction ids

- [ ] **Step 5: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_backend_conformance -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add durable backend conformance model tests"
```

## Wave 9: ObjectTable Rebuild Model Tests

**Purpose:** Verify Zen-style volatile object-index rebuild independently of live transaction execution.

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs` if object winner construction helpers are needed
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Define object-record model**

Use:

```rust
#[derive(Clone, Debug)]
struct ModelObjectRecord {
    object_id: u64,
    version: u32,
    kind: u16,
    field: i32,
    committed: bool,
}
```

- [ ] **Step 2: Add deterministic rebuild tests**

Tests:

- rebuild selects latest committed object version
- rebuild ignores loose-end object publication
- rebuild preserves object id as runtime identity
- rebuild rejects corrupt payload/header mismatch

- [ ] **Step 3: Add generated rebuild tests**

Generate object records and compare rebuilt `ObjectTable` payloads to highest committed version per `ObjectId`.

- [ ] **Step 4: Verify and commit**

Run:

```bash
cargo test -p wasmtime --lib model_object_rebuild -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Commit:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/persist.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add object table rebuild model tests"
```

## Wave 10: WAST And Model Cross-Checks

**Purpose:** Keep WAST coverage and model coverage aligned.

**Files:**
- Modify: `docs/shisoft/transactional-wasm-wast-ledger.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Add a model-test coverage table**

For each transaction feature, record:

- WAST file coverage
- Rust example test coverage
- model/property test coverage
- remaining missing coverage

Features:

- `tmemory.load/store`
- `tmemory.size/grow`
- `tglobal.get/set`
- `ttable.get/set/size/grow/copy/init/fill`
- SIMD `v128.tload/tstore`
- object structs/arrays/i31/extern refs
- permissions
- conflict/concurrency helpers
- durable recovery

- [ ] **Step 2: Gate the transaction WAST suite in verification**

Keep this command as the final model-test integration gate:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected current baseline:

```text
test result: ok. 173 passed; 0 failed; 0 ignored
```

- [ ] **Step 3: Verify and commit**

Run:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Commit:

```bash
git add docs/shisoft/transactional-wasm-wast-ledger.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document transaction model test coverage"
```

## Wave 11: Future Loom Workstream

**Purpose:** Prepare for real thread interleaving tests after shared concurrency primitives are introduced.

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`

- [ ] **Step 1: Add loom entry criteria**

Do not add `loom` until these are true:

- lock ownership is stored in thread-safe shared runtime structures
- tests can run the lock table without invoking Wasmtime compilation
- transaction ids can be assigned deterministically per simulated thread
- no test depends on actual sleeps or wall-clock timing

- [ ] **Step 2: Define first loom scenario**

The first loom scenario should be:

- thread 1 acquires write on granule A
- thread 2 attempts write on granule A
- one owner remains
- aborted transaction releases any partially acquired independent granules

- [ ] **Step 3: Keep this wave documentation-only until entry criteria are met**

Run:

```bash
cargo fmt --check
git diff --check
```

Commit:

```bash
git add docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md
git commit -m "Document future loom transaction test criteria"
```

## Execution Order

Use this order:

1. Wave 1 durable log reference model
2. Wave 2 crash cutpoints
3. Wave 3 mixed file-backed participants
4. Wave 4 version/corruption behavior
5. Wave 5 lock state-space
6. Wave 6 permission state
7. Wave 8 backend conformance
8. Wave 9 object-table rebuild
9. Wave 7 transaction boundary
10. Wave 10 WAST/model cross-check
11. Wave 11 future loom criteria

Rationale:

- Recovery and mixed persistence are most important for the Zen-style agenda.
- Lock and permission models are next because they guard correctness during execution.
- Boundary and WAST cross-checks come after the low-level invariants are stable.
- Loom waits until the runtime has real shared thread-safe state to explore.

## Final Verification Gate

After each group of waves, run:

```bash
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib block_region -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo fmt --check
git diff --check
```

The expected current WAST gate remains:

```text
test result: ok. 173 passed; 0 failed; 0 ignored
```

## Definition Of Done

This roadmap is complete when:

- durable log recovery has deterministic and generated model tests
- mixed file-backed tmemory/object commit and crash recovery have generated model tests
- `LockBased` has deterministic bounded state-space tests and generated schedule tests
- permission transitions have a reference state machine test
- object-table rebuild has generated highest-version recovery tests
- corruption cases reject invalid recovery images intentionally
- WAST coverage and model coverage are mapped in the ledger
- all final verification commands pass

