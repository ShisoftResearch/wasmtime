# File-Backed Transaction Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the transaction-owned durable-log scaffold with a block-region
backend option that can persist transaction data/log records in a file-backed
region using the existing Zen-style fixed log-entry layout.

**Architecture:** Keep `TransactionState` as the durable-log owner. Add a
`TxDurableLog` storage variant backed by `FileBackedMemoryBlockRegion`, and
teach it to implement the same `DurableSink` ordering contract as the in-memory
variant: data append, data flush, fence, log append, log flush, fence, final LP.
Recovery continues to scan the block-region image using existing
`recover_region`.

**Tech Stack:** Rust, Wasmtime runtime internals, `tmemory::block_region`,
existing `TxLogEntry`/`TxDataRecordHeader` codecs, file-backed mmap with
`msync`/`sync_data`.

---

### Task 1: Expose Block-Region Append Primitives

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [x] Add append-log-entry support to block-region streams.
- [x] Track current log block and log sequence in `StreamState`.
- [x] Add flush helpers for data chunks and log blocks.
- [x] Keep recovery-compatible block headers and CRC-sealed entries.

### Task 2: Add File-Backed `TxDurableLog`

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add a block-region-backed storage variant to `TxDurableLog`.
- [x] Implement `DurableSink` for the file-backed variant using region append
      primitives.
- [x] Preserve the existing in-memory test/research variant.

### Task 3: Tests and Docs

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] Add a file-backed durable-log test that publishes two undo records and
      one LP, reopens the region, and verifies recovery sees the transaction as
      committed.
- [x] Add a loose-end undo test proving recovery emits rollback actions when
      the LP is absent.
- [x] Add a reopen/append test proving a file-backed log can rebuild append
      cursors from the existing image.
- [x] Update docs to distinguish the implemented file-backed log from future
      PMEM hardware validation.
- [x] Run focused tests, transaction WAST, `cargo fmt --check`, and
      `git diff --check`.
