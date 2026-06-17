# Persistent GC Migrated Wasmtime Tests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** migrate applicable Wasmtime GC graph/liveness tests into persistent GC tests over durable `ObjectId` graphs.

**Architecture:** Keep this test-only and colocated with the existing persistent GC tests in `crates/wasmtime/src/runtime/transaction.rs`. Adapt Wasmtime's volatile GC graph shapes into persistent structs/arrays and validate marker, sweep, incremental maintenance, and file-backed recovery behavior.

**Tech Stack:** Rust unit tests, `ObjectTable`, `TransactionState`, persistent object layouts, file-backed durable transaction log helpers.

---

### Task 1: Linked-List and Deep-Chain Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add `persistent_gc_migrated_linked_list_survives_incremental_collection`, adapted from `tests/misc_testsuite/gc/linked-list.wast`.
- [x] Add `persistent_gc_migrated_deep_struct_chain_survives_maintenance_steps`, adapted from `tests/misc_testsuite/gc/deeply-nested-struct-survives-gc.wast`.
- [x] Verify with `cargo test -p wasmtime --lib persistent_gc_migrated_ -- --format terse`.

### Task 2: Binary-Tree and Array Reference Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add `persistent_gc_migrated_binary_tree_marks_complete_closure`, adapted from `tests/misc_testsuite/gc/binary-tree.wast`.
- [x] Add `persistent_gc_migrated_array_refs_trace_live_elements_only`, adapted from Wasmtime array GC reference coverage.
- [x] Verify with `cargo test -p wasmtime --lib persistent_gc_migrated_ -- --format terse`.

### Task 3: Pressure and File-Backed Recovery Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [x] Add `persistent_gc_migrated_pressure_sweeps_unrooted_objects`.
- [x] Add `file_backed_persistent_gc_migrated_graph_recovers_only_reachable`.
- [x] Verify with `cargo test -p wasmtime --lib persistent_gc_migrated_ -- --format terse` and `cargo test -p wasmtime --test transaction_persistence -- --format terse`.

### Task 4: Final Verification

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] Record the migrated-test milestone in the implementation log.
- [x] Run `cargo fmt --check`.
- [x] Run `git diff --check`.
- [x] Run `cargo test -p wasmtime --lib persistent_gc -- --format terse`.
- [x] Run `cargo test -p wasmtime --lib persistent_mark_sweep -- --format terse`.
- [x] Run `cargo test -p wasmtime --lib persistent_object_marker -- --format terse`.
