# Transactional Object File-Backed Commit Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire transactional object publications into the existing file-backed transaction log commit path.

**Architecture:** Object payload commit already produces `PendingPublication` records. The runtime commit coordinator should publish those records to `TxDurableLog` before the final LP, then publish exactly one LP shared with any tmemory undo records in the same transaction. The durable log stream is unified for ordering, but file-backed object data records and linear-memory undo data records are allocated from separate data streams.

**Tech Stack:** Rust, Wasmtime runtime, transactional `TxDurableLog`, file-backed block-region recovery tests.

---

### Task 1: Add Non-Final Object Publication Publishing

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [x] Write a failing unit test showing that object publication records can be appended without an LP and are not recovered as committed object winners.
- [x] Add `StreamPublisher::publish_object_publication_before_commit`, returning `PendingCommitLogEntry` with role `TObjectPub`.
- [x] Keep `StreamPublisher::publish_transaction` as the object-only convenience wrapper; runtime commit uses the new non-final publication path and the coordinator-owned LP.
- [x] Route file-backed `TObjectPub` and `TMemoryUndo` data records to separate data streams while keeping log entries in the transaction stream.
- [x] Verify `cargo test -p wasmtime --lib transaction::persist -- --format terse`.

### Task 2: Expose TransactionState Object Publication Commit Hook

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Write a failing file-backed test that stages a persistent struct update, commits object payloads into publications, publishes the object marker through `TransactionState`, publishes the LP, and recovers the object winner.
- [x] Add `TransactionState::publish_object_publications_before_commit`.
- [x] Add a test-only constructor for `TransactionState` with an injected `TxDurableLog`.
- [x] Verify `cargo test -p wasmtime --lib transaction_object -- --format terse`.

### Task 3: Use One Durable LP in Runtime Commit

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [x] Change `commit_staged_tmemory_records` to return `Option<PendingCommitLogEntry>` instead of publishing the LP internally.
- [x] In `transaction_commit_impl`, collect object publications through `commit_object_payloads_into`, publish them through `TransactionState`, and publish one final LP from the last durable marker.
- [x] Keep volatile `VMemory` transactions behavior unchanged.
- [x] Verify targeted transaction, tmemory, and WAST tests.

### Task 4: Document Status

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] Note that persistent object commit now writes `TObjectPub` records through the same durable log stream as tmemory.
- [x] Note the remaining GC-dependent object work remains separate from the durable commit path.
