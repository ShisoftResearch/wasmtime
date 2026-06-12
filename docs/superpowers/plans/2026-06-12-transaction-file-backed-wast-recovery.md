# Transaction File-Backed WAST Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real end-to-end WAST tests that execute transactional WebAssembly against file-backed `tmemory`, restart the store, run recovery, and assert recovered state from WebAssembly.

**Architecture:** The first wave keeps WAST syntax real and puts only restart/recovery orchestration in Rust. A hidden transaction-test embedding hook configures store-local file-backed `tmemory` and a file-backed durable transaction log; a root integration test runs phase-A WAST, drops the store, replays recovery into the file-backed `tmemory`, then runs phase-B WAST against the same storage path.

**Tech Stack:** Rust integration tests, `wasmtime-wast`, Wasmtime `Store`/`Config`, transaction-feature-gated file-backed `TMemory`, file-backed `TxDurableLog`, transaction proposal `.wast` files.

---

## File Structure

- Modify `crates/wasmtime/src/runtime/transaction.rs`
  - Add create/open durable-log configuration methods on `TransactionState`.
  - Add existing-file mode for `TransactionConfig` / `TMemoryFileBacking`.
- Modify `crates/wasmtime/src/runtime/store.rs`
  - Store a transaction memory config next to `TransactionState`.
  - Add hidden `Store` methods for file-backed transaction persistence test setup.
- Modify `crates/wasmtime/src/runtime/vm/instance.rs`
  - Build `TMemory` sidecars from the store's transaction config instead of hard-coded defaults.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Let `TMemory` open existing file-backed storage without truncating it.
  - Promote the recovered-undo application helper from cfg-test-only to crate-internal transaction recovery support.
- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add a `FileBackedRegionMode::OpenExistingPath` mode for non-truncating mappings.
- Modify `crates/wasmtime/src/lib.rs`
  - Add a hidden `_internal::transaction_persistence::recover_file_backed_tmemory_for_test` helper that replays loose-end undo records into a file-backed `tmemory` image.
- Create `tests/transaction_persistence_wast.rs`
  - Dedicated Rust harness for phase-A/phase-B WAST execution with a real restart boundary.
- Create `tests/transaction-persistence/tmemory-commit-before.wast`
  - Phase A module: real `tmemory`, real `tfunc`, real `i32.tstore`, and in-process assertion.
- Create `tests/transaction-persistence/tmemory-commit-after.wast`
  - Phase B module: real `tmemory`, real `tfunc`, real `i32.tload`, and recovered assertion.
- Create `tests/transaction-persistence/tmemory-abort-before.wast`
  - Phase A module: write one committed baseline value, then trap after staging a different value.
- Create `tests/transaction-persistence/tmemory-abort-after.wast`
  - Phase B module: assert the baseline survived and the trapped transaction was not recovered.

## Task 1: Store-Level File-Backed Transaction Configuration

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Modify: `crates/wasmtime/src/runtime/vm/instance.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Write failing tests**

Add tests in `crates/wasmtime/src/runtime/store.rs`:

```rust
#[cfg(all(test, feature = "transaction", unix))]
mod transaction_file_backed_tests {
    use super::*;
    use crate::runtime::vm::TMemory;

    #[test]
    fn store_transaction_file_backed_config_selects_existing_tmemory_path() {
        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("phase.tmemory");
        let tx_log_path = dir.path().join("tx-log.bin");
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());

        store
            .transaction_create_file_backed_storage_for_test(
                tmemory_path.clone(),
                tx_log_path.clone(),
                32,
            )
            .unwrap();
        let create_config = store.as_store_opaque().transaction_config().clone();
        let mut created = TMemory::new(create_config, 1, Some(1)).unwrap();
        created.commit_range(64, &[1, 2, 3, 4]).unwrap();
        drop(created);

        store
            .transaction_open_file_backed_storage_for_test(tmemory_path.clone(), tx_log_path)
            .unwrap();
        let open_config = store.as_store_opaque().transaction_config().clone();
        let reopened = TMemory::new(open_config, 1, Some(1)).unwrap();

        assert_eq!(reopened.read_committed(64..68).unwrap(), vec![1, 2, 3, 4]);
    }
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p wasmtime --lib store_transaction_file_backed_config_selects_existing_tmemory_path -- --format terse
```

Expected: FAIL because the hidden store configuration methods and existing-file mode do not exist.

- [ ] **Step 3: Implement minimal configuration support**

Implement these signatures:

```rust
impl TransactionConfig {
    pub(crate) fn with_file_backed_tmemory_existing_path(path: PathBuf) -> Result<Self>;
}

impl TransactionState {
    pub(crate) fn create_file_backed_durable_log(&mut self, path: &Path, num_blocks: u32) -> Result<()>;
    pub(crate) fn open_file_backed_durable_log(&mut self, path: &Path) -> Result<()>;
}

impl StoreOpaque {
    pub(crate) fn transaction_config(&self) -> &TransactionConfig;
}

impl<T> Store<T> {
    #[doc(hidden)]
    #[cfg(feature = "transaction")]
    pub fn transaction_create_file_backed_storage_for_test(
        &mut self,
        tmemory_path: PathBuf,
        tx_log_path: PathBuf,
        tx_log_blocks: u32,
    ) -> Result<()>;

    #[doc(hidden)]
    #[cfg(feature = "transaction")]
    pub fn transaction_open_file_backed_storage_for_test(
        &mut self,
        tmemory_path: PathBuf,
        tx_log_path: PathBuf,
    ) -> Result<()>;
}
```

Add a `transaction_config: TransactionConfig` field to `StoreOpaque`, initialize it with `TransactionConfig::default()`, and have `Instance::build_tmemory_sidecar` receive and clone this config when creating `TMemory`.

Add `TMemoryFileBacking::ExistingPath(PathBuf)` and `FileBackedRegionMode::OpenExistingPath(PathBuf)` so phase-B instantiation does not truncate the `tmemory` file.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib store_transaction_file_backed_config_selects_existing_tmemory_path -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/store.rs crates/wasmtime/src/runtime/vm/instance.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Wire file-backed transaction storage into stores"
```

## Task 2: Hidden Recovery Helper For File-Backed TMemory

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/lib.rs`

- [ ] **Step 1: Write failing integration-facing test**

Add a test in `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`:

```rust
#[cfg(all(test, unix))]
#[test]
fn file_backed_recovery_helper_replays_loose_end_undo_to_existing_tmemory_file() {
    use crate::runtime::transaction::{TransactionConfig, TxDurableLog};

    let dir = tempfile::tempdir().unwrap();
    let tmemory_path = dir.path().join("recover.tmemory");
    let tx_log_path = dir.path().join("recover-log.bin");
    let mut memory = TMemory::new(
        TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
        1,
        Some(1),
    )
    .unwrap();
    memory.commit_range(64, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();
    let undo = memory
        .prepare_tmemory_undo_record(Some(0), 0, 1, &[0x11; TMEMORY_GRANULE_SIZE])
        .unwrap();
    memory
        .commit_staged_tmemory_granule(1, &[0x11; TMEMORY_GRANULE_SIZE])
        .unwrap();
    drop(memory);

    let mut log = TxDurableLog::create_file_backed(&tx_log_path, 32).unwrap();
    let mut sink = log.stream_sink(41);
    let mut publisher = crate::runtime::transaction::StreamPublisher::new(&mut sink, 41, 41);
    publisher.publish_tmemory_undo(&undo).unwrap();
    drop(log);

    crate::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )
    .unwrap();

    let reopened = TMemory::new(
        TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path).unwrap(),
        1,
        Some(1),
    )
    .unwrap();
    assert_eq!(reopened.read_committed(64..68).unwrap(), vec![0xaa, 0xbb, 0xcc, 0xdd]);
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test -p wasmtime --lib file_backed_recovery_helper_replays_loose_end_undo_to_existing_tmemory_file -- --format terse
```

Expected: FAIL because `_internal::transaction_persistence::recover_file_backed_tmemory_for_test` is not implemented and the undo replay helper is cfg-test-only.

- [ ] **Step 3: Implement helper**

Expose a hidden helper under `#[cfg(all(feature = "runtime", feature = "transaction"))]`:

```rust
pub fn recover_file_backed_tmemory_for_test(
    tx_log_path: &std::path::Path,
    tmemory_path: &std::path::Path,
    min_pages: u64,
    max_pages: Option<u64>,
) -> crate::Result<TransactionPersistenceRecoveredRegion>
```

Implementation:

1. Call `reopen_and_recover_file_backed_region(tx_log_path)?`.
2. Create a `TMemory` with `TransactionConfig::with_file_backed_tmemory_existing_path(tmemory_path.to_path_buf())?`.
3. Apply `recovered.tmemory_undo_rollbacks.clone()` to that `TMemory`.
4. Return the recovered summary.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test -p wasmtime --lib file_backed_recovery_helper_replays_loose_end_undo_to_existing_tmemory_file -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/lib.rs
git commit -m "Add file-backed tmemory recovery helper"
```

## Task 3: Real WAST Restart/Recovery Integration Test

**Files:**
- Create: `tests/transaction_persistence_wast.rs`
- Create: `tests/transaction-persistence/tmemory-commit-before.wast`
- Create: `tests/transaction-persistence/tmemory-commit-after.wast`

- [ ] **Step 1: Add failing WAST fixtures**

Create `tests/transaction-persistence/tmemory-commit-before.wast`:

```wasm
(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )

  (tfunc (export "write") (param $value i32)
    (i32.tstore (i32.const 64) (local.get $value))
  )
)

(assert_return (tinvoke "read") (i32.const 0))
(assert_return (tinvoke "write" (i32.const 0x11223344)))
(assert_return (tinvoke "read") (i32.const 0x11223344))
```

Create `tests/transaction-persistence/tmemory-commit-after.wast`:

```wasm
(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )
)

(assert_return (tinvoke "read") (i32.const 0x11223344))
```

Create `tests/transaction_persistence_wast.rs`:

```rust
use std::path::Path;
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_wast::{Async, WastContext};

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_bulk_memory(true);
    config.wasm_gc(true);
    config.wasm_reference_types(true);
    config.wasm_function_references(true);
    config.wasm_tail_call(true);
    Engine::new(&config)
}

fn run_wast_phase(
    engine: &Engine,
    wast: &Path,
    tmemory_path: &Path,
    tx_log_path: &Path,
    create: bool,
) -> Result<()> {
    let mut context = WastContext::new(engine, Async::No, |_| {});
    if create {
        context.core_store.transaction_create_file_backed_storage_for_test(
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
            64,
        )?;
    } else {
        context.core_store.transaction_open_file_backed_storage_for_test(
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
        )?;
    }
    context.run_file(wast)
}

#[test]
#[cfg(unix)]
fn transaction_wast_commit_survives_file_backed_recovery() -> Result<()> {
    let _ = env_logger::try_init();
    let dir = tempfile::tempdir()?;
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let engine = engine()?;

    run_wast_phase(
        &engine,
        Path::new("tests/transaction-persistence/tmemory-commit-before.wast"),
        &tmemory_path,
        &tx_log_path,
        true,
    )?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;

    run_wast_phase(
        &engine,
        Path::new("tests/transaction-persistence/tmemory-commit-after.wast"),
        &tmemory_path,
        &tx_log_path,
        false,
    )
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test --test transaction_persistence_wast transaction_wast_commit_survives_file_backed_recovery -- --format terse
```

Expected: FAIL before Task 1 and Task 2 implementation. If Tasks 1 and 2 are already complete, temporarily run before adding production changes for this task and expect failure from missing fixtures or post-recovery assertion reading `0`.

- [ ] **Step 3: Implement any missing runner glue**

Keep the runner small:

- no custom WAST directives,
- no proposal WAST normalization,
- no mocked return values,
- all assertions stay in the `.wast` files.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test --test transaction_persistence_wast transaction_wast_commit_survives_file_backed_recovery -- --format terse
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/transaction_persistence_wast.rs tests/transaction-persistence/tmemory-commit-before.wast tests/transaction-persistence/tmemory-commit-after.wast
git commit -m "Add file-backed transaction recovery WAST test"
```

## Task 4: Trap/Abort Restart WAST Coverage

**Files:**
- Modify: `tests/transaction_persistence_wast.rs`
- Create: `tests/transaction-persistence/tmemory-abort-before.wast`
- Create: `tests/transaction-persistence/tmemory-abort-after.wast`

- [ ] **Step 1: Add failing abort fixtures**

Create `tests/transaction-persistence/tmemory-abort-before.wast`:

```wasm
(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )

  (tfunc (export "write") (param $value i32)
    (i32.tstore (i32.const 64) (local.get $value))
  )

  (tfunc (export "trap-after-stage")
    (i32.tstore (i32.const 64) (i32.const 0x55667788))
    unreachable
  )
)

(assert_return (tinvoke "write" (i32.const 0x11223344)))
(assert_return (tinvoke "read") (i32.const 0x11223344))
(assert_trap (tinvoke "trap-after-stage") "unreachable")
(assert_return (tinvoke "read") (i32.const 0x11223344))
```

Create `tests/transaction-persistence/tmemory-abort-after.wast`:

```wasm
(module
  (tmemory 1)

  (tfunc (export "read") (result i32)
    (i32.tload (i32.const 64))
  )
)

(assert_return (tinvoke "read") (i32.const 0x11223344))
```

Add to `tests/transaction_persistence_wast.rs`:

```rust
#[test]
#[cfg(unix)]
fn transaction_wast_trap_does_not_recover_staged_tmemory_write() -> Result<()> {
    let _ = env_logger::try_init();
    let dir = tempfile::tempdir()?;
    let tmemory_path = dir.path().join("tmemory.bin");
    let tx_log_path = dir.path().join("tx-log.bin");
    let engine = engine()?;

    run_wast_phase(
        &engine,
        Path::new("tests/transaction-persistence/tmemory-abort-before.wast"),
        &tmemory_path,
        &tx_log_path,
        true,
    )?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;

    run_wast_phase(
        &engine,
        Path::new("tests/transaction-persistence/tmemory-abort-after.wast"),
        &tmemory_path,
        &tx_log_path,
        false,
    )
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test --test transaction_persistence_wast transaction_wast_trap_does_not_recover_staged_tmemory_write -- --format terse
```

Expected: FAIL if abort/restart fixture wiring is missing or post-recovery instantiation truncates storage.

- [ ] **Step 3: Implement missing behavior only if needed**

The expected implementation is usually already covered by Task 1 and Task 2. If this fails, fix the real abort/recovery path rather than changing WAST assertions.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test --test transaction_persistence_wast -- --format terse
```

Expected: PASS for both persistence WAST tests.

- [ ] **Step 5: Commit**

```bash
git add tests/transaction_persistence_wast.rs tests/transaction-persistence/tmemory-abort-before.wast tests/transaction-persistence/tmemory-abort-after.wast
git commit -m "Add abort recovery transaction WAST test"
```

## Task 5: Final Verification And Documentation

**Files:**
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Document coverage**

Append a short entry explaining:

- end-to-end WAST recovery now exists for file-backed linear `tmemory`,
- restart/recovery is orchestrated by Rust because WAST has no restart directive,
- object redo/root recovery WAST remains future work until persistent-object runtime publication is fully embedder-wired.

- [ ] **Step 2: Verify full focused suite**

Run:

```bash
cargo test --test transaction_persistence_wast -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo fmt --check
git diff --check
```

Expected:

- `transaction_persistence_wast`: all tests pass,
- `transaction`: all tests pass,
- `tmemory`: all tests pass,
- transaction proposal WAST: `173 passed; 0 failed; 0 ignored`,
- formatting and whitespace checks pass.

- [ ] **Step 3: Commit**

```bash
git add docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document file-backed transaction WAST recovery coverage"
```

## Follow-Up Scope Not In This Wave

- End-to-end WAST for persistent `tstruct`/`tarray` redo recovery.
- End-to-end WAST for `tglobal` and `ttable` persistent object roots after a restart.
- Multi-memory restart tests if `tmemory.grow` changes the live page count across recovery.
- Loom/shared-thread recovery ordering tests.
