# TMemory Undo-In-Place Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the durable substrate for persistent linear-memory undo logging so `tmemory` can move from COW redo-style staging to undo-before-in-place persistence.

**Architecture:** Object records remain COW/redo publication data and are recovered by highest committed version per logical id. Linear-memory records use a separate undo role: old granule bytes are logged before persistent in-place mutation, committed transactions discard undo records during recovery, and loose-end transactions produce rollback actions. The first implementation lands the codecs, publication ordering, and recovery classification before changing the compiled `tstore` pointer path.

**Tech Stack:** Rust, Wasmtime runtime internals, `crates/wasmtime/src/runtime/vm/memory/tmemory`, `crates/wasmtime/src/runtime/transaction/persist.rs`, unit tests with `cargo test -p wasmtime --lib`.

---

### Task 1: Encode Log Entry Roles

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`

- [x] **Step 1: Add a failing test**

Add a unit test that creates a `TxLogEntry`, marks it as `TMemoryUndo`, seals it, and verifies that the role round-trips through the CRC-protected entry.

- [x] **Step 2: Run the test to verify failure**

Run:

```bash
cargo test -p wasmtime --lib tmemory_undo_log_entry_role_roundtrips -- --format terse
```

Expected: failure because `TxLogEntryRole` and role helpers do not exist yet.

- [x] **Step 3: Implement role helpers**

Add `TxLogEntryRole::{TObjectPub, TMemoryUndo}` and store the role in `TxLogEntry.reserved`. Keep `TObjectPub` as zero so existing logs remain object-publication logs.

- [x] **Step 4: Verify**

Run the same test and expect one passing test.

### Task 2: Encode Separate Granule Undo Data Records

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [x] **Step 1: Add failing tests**

Add tests for:

- `PendingGranuleUndo` encoding old granule bytes with `PackedGranuleDomain::TMemory`
- undo records staying distinct from `PendingPublication`

- [x] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p wasmtime --lib tmemory_undo -- --format terse
```

Expected: failure because undo record types do not exist yet.

- [x] **Step 3: Implement undo data record helpers**

Add `PendingGranuleUndo`, `encode_undo_data_record`, and `TMemory::encode_granule_undo_data_record`. Do not reuse `PendingPublication` for undo records.

- [x] **Step 4: Verify**

Run the same focused test and expect the new tests to pass.

### Task 3: Publish Undo Before In-Place Mutation

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [x] **Step 1: Add a failing ordering test**

Add a recorder test that expects this event order for a granule undo record:

```text
DataWrite, DataFlush, Fence, LogWrite, LogFlush, Fence
```

The final LP is not written by the per-granule undo barrier; LP is written by transaction commit after modified linear-memory bytes are flushed.

- [x] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p wasmtime --lib tmemory_undo_is_durable_before_in_place_write -- --format terse
```

Expected: failure because the undo barrier publisher does not exist yet.

- [x] **Step 3: Implement the undo barrier publisher**

Add `StreamPublisher::publish_tmemory_undo_before_in_place_write`, append the undo data record, flush/fence data, append a `TMemoryUndo` log entry, flush/fence log.

- [x] **Step 4: Add undo-only LP support**

Add a small commit-marker return value from the undo barrier and
`StreamPublisher::publish_commit_lp` so a transaction that only mutates
persistent `tmemory` can publish LP without appending another data record.

- [x] **Step 5: Verify**

Run the focused test and then:

```bash
cargo test -p wasmtime --lib transaction::persist -- --format terse
```

### Task 4: Recover Loose-End Undo Rollbacks

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [x] **Step 1: Add failing recovery tests**

Add tests for:

- a loose-end transaction with a `TMemoryUndo` entry produces one rollback action
- a committed transaction with a `TMemoryUndo` entry and LP produces no rollback action
- publication winner recovery ignores undo records

- [x] **Step 2: Run tests to verify failure**

Run:

```bash
cargo test -p wasmtime --lib recovery_replays_tmemory_undo -- --format terse
```

Expected: failure because recovery has no rollback action collection yet.

- [x] **Step 3: Implement recovery classification**

Add `RecoveredTMemoryUndoRollback` and store it in `RecoveredRegion`. During stream replay, committed transaction entries go through the existing winner path, except undo entries are ignored. Loose-end undo entries become rollback actions with their old-byte payload locations.

- [x] **Step 4: Verify**

Run the focused recovery tests and:

```bash
cargo test -p wasmtime --lib tmemory -- --format terse
```

### Task 5: Runtime Write Path Follow-Up

**Files:**
- Modify later: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify later: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify later: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Design direct mutable access**

Add a safe helper that returns a bounded mutable slice/pointer into persistent `tmemory` only after durable undo has completed.

- [ ] **Step 2: Move persistent backends to undo-in-place**

Keep `VMemory` on the existing staged path if useful for tests, and route `NVMemory`/`FileBackedMemory` through the undo barrier before direct writes.

- [ ] **Step 3: Add abort/crash recovery integration**

Abort restores old bytes from transaction-local undo copies. Restart recovery applies `RecoveredTMemoryUndoRollback` records for transactions without LP.

- [ ] **Step 4: Verify full WAST stability**

Run:

```bash
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```
