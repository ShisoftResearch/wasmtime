# Mock Transaction Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make supported simple-transactions proposal WAST tests run through the real Wasmtime engine using a mock transactional runtime backed by ordinary Wasmtime memory.

**Architecture:** Keep ordinary Wasmtime memory/global allocation unchanged. Transaction operators continue to lower to transaction libcalls; those libcalls use `TransactionState` for staged copy-on-write writes, deterministic `TMEMORY_GRANULE_SIZE` conflict checks, and commit/abort behavior. Unsupported proposal features remain disabled in the WAST harness until their semantics exist.

**Tech Stack:** Rust, Wasmtime runtime VM libcalls, `wasmtime-environ` transaction metadata, local patched wasm-tools `wast`/`wat`, existing WAST test harness.

**Mock Tag Policy:** Every intentional mock or scaffold added by this roadmap
must include the grep-able marker `SHISOFT-TWASM-MOCK` with a short note about
the future real subsystem that replaces it. Run `rg "SHISOFT-TWASM-MOCK"` to
audit replacement work.

---

## File Map

- `crates/wasmtime/src/runtime/transaction.rs`
  - Owns transaction state, staging records, granule conflict bookkeeping, and unit tests for copy-on-write helpers.
- `crates/wasmtime/Cargo.toml`
  - Add `wat = { workspace = true }` as a dev-dependency for readable runtime tests that use transaction text syntax.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Implements transaction memory/global libcalls called by compiled Wasm.
- `crates/wasmtime/src/runtime/vm/instance.rs`
  - Already exposes `get_defined_memory[_mut]` and `global_ptr`; use these, do not add new API unless borrow-checking forces it.
- `crates/wasmtime/src/runtime/vm/memory.rs`
  - Already exposes `Memory::byte_size` and `Memory::grow`; use ordinary memory as temporary `tmemory` backing.
- `crates/test-util/src/wast.rs`
  - Moves WAST files from normalized/ignored mode to real parser and real execution as runtime support lands.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record each tranche enabled and the verification command/result.

## Task 1: TransactionState Byte-Range Overlay Helpers

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add failing tests for staged byte-range reads and writes**

Add tests inside `#[cfg(test)] mod tests`:

```rust
#[test]
fn staged_memory_write_overlays_backing_bytes() {
    let mut state = TransactionState::default();
    state.begin().unwrap();

    let mut backing = vec![0u8; 512];
    backing[260..264].copy_from_slice(&[1, 2, 3, 4]);
    state
        .stage_memory_write(0, 258, &[9, 8, 7, 6], &backing)
        .unwrap();

    let bytes = state.read_memory_overlay(0, 256, 12, &backing).unwrap();
    assert_eq!(bytes, vec![0, 0, 9, 8, 7, 6, 3, 4, 0, 0, 0, 0]);
}

#[test]
fn staged_memory_write_across_granules_commits_as_granules() {
    let mut state = TransactionState::default();
    state.begin().unwrap();

    let backing = vec![0u8; 512];
    state
        .stage_memory_write(0, 254, &[1, 2, 3, 4], &backing)
        .unwrap();

    let mut records = Vec::new();
    state.commit_with(|record| {
        records.push(record.clone());
        Ok(())
    }).unwrap();

    assert_eq!(records.len(), 2);
    assert!(matches!(
        &records[0],
        StagedRecord::MemoryGranule { memory_index: 0, granule_index: 0, .. }
    ));
    assert!(matches!(
        &records[1],
        StagedRecord::MemoryGranule { memory_index: 0, granule_index: 1, .. }
    ));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib staged_memory_write
```

Expected: fails because `stage_memory_write` and `read_memory_overlay` do not exist.

- [ ] **Step 3: Implement byte-range helpers**

Add helpers to `impl TransactionState`:

```rust
pub(crate) fn stage_memory_write(
    &mut self,
    memory_index: u32,
    addr: u64,
    bytes: &[u8],
    backing: &[u8],
) -> Result<()> {
    self.ensure_active()?;
    let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
    let end = start
        .checked_add(bytes.len())
        .context("tmemory access range overflow")?;
    ensure!(end <= backing.len(), "out of bounds tmemory access");

    let mut cursor = start;
    while cursor < end {
        let granule_index = (cursor / 256) as u64;
        let granule_start = (granule_index as usize) * 256;
        let granule_end = granule_start.saturating_add(256).min(backing.len());
        let copy_start = cursor;
        let copy_end = end.min(granule_end);
        let src_start = copy_start - start;
        let src_end = copy_end - start;

        let key = MemoryGranuleKey { memory_index, granule_index };
        let granule = self
            .staged_memory_granules
            .entry(key)
            .or_insert_with(|| backing[granule_start..granule_end].to_vec());
        granule[copy_start - granule_start..copy_end - granule_start]
            .copy_from_slice(&bytes[src_start..src_end]);
        self.memory_read_granules.insert(key);
        self.memory_write_granules.insert(key);
        cursor = copy_end;
    }

    Ok(())
}

pub(crate) fn read_memory_overlay(
    &mut self,
    memory_index: u32,
    addr: u64,
    len: usize,
    backing: &[u8],
) -> Result<Vec<u8>> {
    self.ensure_active()?;
    let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
    let end = start
        .checked_add(len)
        .context("tmemory access range overflow")?;
    ensure!(end <= backing.len(), "out of bounds tmemory access");

    let mut result = backing[start..end].to_vec();
    let mut cursor = start;
    while cursor < end {
        let granule_index = (cursor / 256) as u64;
        let granule_start = (granule_index as usize) * 256;
        let granule_end = granule_start.saturating_add(256).min(backing.len());
        let copy_start = cursor;
        let copy_end = end.min(granule_end);

        self.memory_read_granules.insert(MemoryGranuleKey {
            memory_index,
            granule_index,
        });

        if let Some(granule) = self.staged_memory_granule(memory_index, granule_index) {
            result[copy_start - start..copy_end - start]
                .copy_from_slice(&granule[copy_start - granule_start..copy_end - granule_start]);
        }
        cursor = copy_end;
    }

    Ok(result)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib staged_memory_write
```

Expected: both new tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Add transaction byte overlay helpers"
```

## Task 2: Mock Memory Libcalls

**Files:**
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Add `wat` as a dev-dependency for runtime tests**

In `crates/wasmtime/Cargo.toml`, add:

```toml
[dev-dependencies]
wat = { workspace = true }
```

Keep the existing dev-dependencies in the same section; add only the new `wat`
entry.

- [ ] **Step 2: Add failing real-engine tests for commit and abort**

Add tests in `crates/wasmtime/src/runtime/transaction.rs` using WAT through the patched parser:

```rust
fn transaction_test_module(engine: &crate::Engine, wat: &str) -> crate::Module {
    crate::Module::new(engine, wat::parse_str(wat).unwrap()).unwrap()
}

#[test]
fn mock_transaction_store_commits_to_memory() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (tmemory 1)
          (func (export "write")
            (ttry)
            (i32.tstore (i32.const 0) (i32.const 42)))
          (func (export "read") (result i32)
            (i32.load (i32.const 0))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write = instance.get_typed_func::<(), ()>(&mut store, "write").unwrap();
    let read = instance.get_typed_func::<(), i32>(&mut store, "read").unwrap();

    write.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 42);
}

#[test]
fn mock_transaction_fail_discards_memory_write() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (tmemory 1)
          (func (export "write_fail")
            (ttry)
            (i32.tstore (i32.const 0) (i32.const 42))
            (tfail))
          (func (export "read") (result i32)
            (i32.load (i32.const 0))))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let write_fail = instance.get_typed_func::<(), ()>(&mut store, "write_fail").unwrap();
    let read = instance.get_typed_func::<(), i32>(&mut store, "read").unwrap();

    write_fail.call(&mut store, ()).unwrap();

    assert_eq!(read.call(&mut store, ()).unwrap(), 0);
}
```

- [ ] **Step 3: Run tests to verify they fail on unimplemented libcalls**

Run:

```bash
cargo test -p wasmtime --lib mock_transaction_
```

Expected: tests fail with transaction memory helper not implemented.

- [ ] **Step 4: Add transaction scratch storage**

In `TransactionState`, add:

```rust
scratch: Vec<u8>,
```

Initialize it in `Default`:

```rust
scratch: Vec::new(),
```

Add:

```rust
pub(crate) fn set_scratch(&mut self, bytes: Vec<u8>) -> *mut u8 {
    self.scratch = bytes;
    self.scratch.as_mut_ptr()
}
```

The scratch pointer is valid until the next transaction libcall mutates the
store's transaction state. Lowered transaction loads consume it immediately, so
that lifetime is sufficient for this mock runtime.

- [ ] **Step 5: Implement memory libcall helpers**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`:

- Use `DefinedMemoryIndex::from_u32(memory)`.
- Use `store.instance_mut(instance).get_defined_memory(_mut)` for memory access.
- Use `memory.byte_size()` for bounds.
- For reads, copy backing bytes into a `Vec<u8>`, overlay staged bytes, store the result in `TransactionState::scratch`, and return `scratch.as_mut_ptr()`.
- For stores, copy the current backing byte range and call `TransactionState::stage_memory_write`.
- For commit, change `transaction_commit` to call `commit_with` and write `StagedRecord::MemoryGranule` bytes back into backing memory.

Required helper shape:

```rust
fn transaction_tmemory_load(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    offset: u64,
    len: u32,
) -> Result<*mut u8> {
    let effective = addr.checked_add(offset).context("tmemory address overflow")?;
    let len = usize::try_from(len).context("tmemory access length overflow")?;
    let memory_index = DefinedMemoryIndex::from_u32(memory);
    let backing = {
        let instance_ref = store.instance_mut(instance);
        let memory = instance_ref.as_ref().get_defined_memory(memory_index);
        read_memory_bytes(memory, effective, len)?
    };
    let bytes = store
        .store_opaque_mut()
        .transaction_state_mut()
        .read_memory_overlay(memory, effective, len, &backing)?;
    Ok(store.store_opaque_mut().transaction_state_mut().set_scratch(bytes))
}
```

Implement this as two short scopes so the mutable instance borrow ends before
calling `store.store_opaque_mut()`:

```rust
let backing = {
    let mut instance_ref = store.instance_mut(instance);
    let memory = instance_ref.as_mut().get_defined_memory_mut(memory_index);
    read_memory_bytes(memory, effective, len)?
};
let state = store.store_opaque_mut().transaction_state_mut();
let bytes = state.read_memory_overlay(memory, effective, len, &backing)?;
Ok(state.set_scratch(bytes))
```

- [ ] **Step 6: Run tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib mock_transaction_
```

Expected: memory commit/abort tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/Cargo.toml crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/transaction.rs
git commit -m "Implement mock transaction memory libcalls"
```

## Task 3: Mock Global Libcalls

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [ ] **Step 1: Add failing tests for `tglobal.get/set`**

Add:

```rust
#[test]
fn mock_transaction_global_set_commits() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (tglobal $g (mut i32) (i32.const 0))
          (func (export "set")
            (ttry)
            (tglobal.set $g (i32.const 7)))
          (func (export "get") (result i32)
            (global.get $g)))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let set = instance.get_typed_func::<(), ()>(&mut store, "set").unwrap();
    let get = instance.get_typed_func::<(), i32>(&mut store, "get").unwrap();

    set.call(&mut store, ()).unwrap();

    assert_eq!(get.call(&mut store, ()).unwrap(), 7);
}

#[test]
fn mock_transaction_global_fail_discards_set() {
    let engine = crate::Engine::default();
    let module = transaction_test_module(
        &engine,
        r#"
        (module
          (tglobal $g (mut i32) (i32.const 0))
          (func (export "set_fail")
            (ttry)
            (tglobal.set $g (i32.const 7))
            (tfail))
          (func (export "get") (result i32)
            (global.get $g)))
        "#,
    );
    let mut store = crate::Store::new(&engine, ());
    let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
    let set_fail = instance.get_typed_func::<(), ()>(&mut store, "set_fail").unwrap();
    let get = instance.get_typed_func::<(), i32>(&mut store, "get").unwrap();

    set_fail.call(&mut store, ()).unwrap();

    assert_eq!(get.call(&mut store, ()).unwrap(), 0);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p wasmtime --lib mock_transaction_global
```

Expected: fails with tglobal helper not implemented.

- [ ] **Step 3: Extend `GlobalSnapshot` and global libcalls**

Extend `GlobalSnapshot` to include scalar floats:

```rust
pub(crate) enum GlobalSnapshot {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
}
```

Implement:

- tag `0` = i32
- tag `1` = i64
- tag `2` = f32 bits
- tag `3` = f64 bits

Use `Instance::global_ptr(module.defined_global_index(GlobalIndex::from_u32(global)).unwrap())`
and `VMGlobalDefinition::{as_i32_mut, as_i64_mut, as_f32_bits_mut, as_f64_bits_mut}` for writes.
For reads, return a pointer to scratch bytes in `TransactionState`.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p wasmtime --lib mock_transaction_global
```

Expected: global commit/fail tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Implement mock transaction global libcalls"
```

## Task 4: Enable First Real WAST Tranches

**Files:**
- Modify: `crates/test-util/src/wast.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 1: Make `tmemory_size.wast` and `tmemory_grow.wast` real-engine tests**

In `transaction_proposal_uses_real_text_parser`, keep:

```rust
matches!(name, "tmemory_size.wast" | "tmemory_grow.wast")
```

Change `WastTest::transaction_proposal_enabled` behavior so real-text parser
files can run once supported instead of being treated as ignored. Update the
unit test name and assertion from "not normalized or run yet" to "uses real
parser and runs".

- [x] **Step 2: Run WAST tests**

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Result:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out
```

- [x] **Step 3: Record result**

Append a dated line to `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
- 2026-06-04: Enabled real-engine `tmemory_size.wast` and `tmemory_grow.wast`
  against the mock transaction runtime. Verification:
  `WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse`.
```

- [x] **Step 4: Commit**

```bash
git add crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Run first transaction WAST files on mock runtime"
```

## Task 5: Add Deterministic Conflict Checks

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`

- [x] **Step 1: Add conflict unit tests**

Add tests that a read lock blocks another transaction's write and a write lock
blocks another transaction's read using existing `LockBased` directly:

```rust
#[test]
fn lock_based_conflicts_are_released_on_abort() {
    let mut locks = LockBased::default();
    let first = TransactionId(1);
    let second = TransactionId(2);

    locks.acquire_memory_granule_write(first, 0, 0).unwrap();
    assert!(locks.acquire_memory_granule_read(second, 0, 0).is_err());

    locks.release_transaction(first);

    locks.acquire_memory_granule_read(second, 0, 0).unwrap();
}
```

- [x] **Step 2: Run tests**

```bash
cargo test -p wasmtime --lib lock_based_conflicts
```

Expected: the new conflict release test passes.

- [x] **Step 3: Wire conflict checks into memory libcalls**

For every granule touched by `transaction_tmemory_load`, call
`LockBased::acquire_memory_granule_read`. For every granule touched by
`transaction_tmemory_store`, call `LockBased::acquire_memory_granule_write`.
If the current store only supports one active transaction at a time, keep the
checks in `TransactionState` and document that inter-store conflicts are
outside the mock runtime scope in `docs/shisoft/transactional-wasm-implementation-log.md`.

- [x] **Step 4: Record conflict WAST scope**

Append to `docs/shisoft/transactional-wasm-implementation-log.md`:

```markdown
- 2026-06-04: Mock runtime conflict checks are deterministic and unit-tested
  through `LockBased`. Real `tconflict-tmemory.wast` remains deferred until the
  runtime has an instance-level transaction object table or another way to model
  conflicts across transaction participants.
```

- [x] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Check mock transaction memory conflicts"
```

## Task 6: Expand Real WAST Coverage

**Files:**
- Modify: `crates/test-util/src/wast.rs`
- Modify: `docs/shisoft/transactional-wasm-wast-ledger.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 1: Enable scalar memory files one at a time**

Try in order:

```rust
"tload.wast"
"tstore.wast"
"tmemory_trap.wast"
```

Add one file at a time to `transaction_proposal_uses_real_text_parser`.

- [x] **Step 2: Verify after each file**

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: no failures before enabling the next file.

Result:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tload.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstore.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory_trap.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out
```

`tmemory_trap.wast` required local `wasm-tools-transaction` fork commit
`0db14108` so `(tdata ...)` parses as a transaction text alias for ordinary
active data segments.

- [x] **Step 3: Update ledger**

For each enabled file, update `docs/shisoft/transactional-wasm-wast-ledger.md`
with:

```markdown
- `tload.wast`: real parser + mock runtime, passing as of 2026-06-04.
- `tstore.wast`: real parser + mock runtime, passing as of 2026-06-04.
- `tmemory_trap.wast`: real parser + mock runtime, passing as of 2026-06-04.
```

- [x] **Step 4: Commit**

```bash
git add crates/test-util/src/wast.rs docs/shisoft/transactional-wasm-wast-ledger.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Expand mock runtime transaction WAST coverage"
```

## Verification Checklist

Run before claiming this plan complete:

```bash
cargo test -p wasmtime --lib transaction
cargo test -p wasmtime-environ transaction_object_metadata
cargo test -p wasmtime-test-util --features wast transaction_proposal
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected:

- Wasmtime transaction unit tests pass.
- Environ metadata tests pass.
- Test-util transaction proposal tests pass.
- Transaction WAST selected tests pass with fewer ignored files as tranches are enabled.
