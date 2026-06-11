# Unified TMemory Durable Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move persistent `tmemory` transaction logging out of individual
`TMemory` objects and into the transaction commit path so one transaction can
atomically touch multiple persistent memories.

**Architecture:** `TMemory` becomes a durable participant that can prepare
undo records, apply staged writes, and flush data ranges. The transaction layer
owns the durable stream, publishes all undo records before any in-place writes,
and emits one final LP after all participant writes are durable.

**Tech Stack:** Rust, Wasmtime runtime internals, existing transactional
`TMemory` durable-log codec and WAST harness.

---

### Task 1: Add Transaction-Owned Durable Sink Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`

- [x] Add a failing runtime test that stages writes to two distinct persistent
      `tmemory` participants and expects one shared commit stream instead of
      the current rejection.
- [x] Add a focused `TMemory` test proving participant helpers can prepare
      undo records without publishing LP from inside `TMemory`.
- [x] Run the targeted tests and verify they fail for the current ownership
      shape.

### Task 2: Split `TMemory` Durable Participant Operations

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [x] Add a transaction-layer durable sink type that can own one stream of data
      records and log entries.
- [x] Add `TMemory` helpers to prepare `PendingGranuleUndo` records, apply
      staged granules in place, and publish no LP internally.
- [x] Keep the existing single-memory helper as a compatibility wrapper only if
      it delegates to the new split operations.

### Task 3: Wire Unified Commit in `libcalls`

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Replace the multi-persistent-`tmemory` guard with participant collection.
- [x] Append all persistent `TMemoryUndo` data/log entries into one transaction
      stream before in-place writes.
- [x] Apply all persistent participant writes and then publish one final LP.
- [x] Keep volatile `VMemory` on the direct in-memory commit path.

### Task 4: Docs and Verification

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] Update the design doc to state that `TMemory` does not own transaction
      logs; the transaction/store layer owns the unified stream.
- [x] Update the implementation log with the new status and remaining
      limitations.
- [x] Run focused unit tests, transaction WAST tests, `cargo fmt --check`, and
      `git diff --check`.
