# Thread-Safe Transaction Region Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the transaction prototype from store-local correctness to a thread-safe persistent-region runtime where multiple host threads and stores can run transactions against the same persistent region.

**Architecture:** Keep live Wasmtime execution state local to each `Store`, but move persistent transaction authority into a shared `Arc<TransactionRegionRuntime>`. The shared runtime owns global transaction ids, granule locks, versions, durable log publication, persistent object index, roots, block allocation, and persistent-GC coordination. Live `VMGcRef` values remain store-local and never cross the shared boundary; shared identity is `ObjectId`/`GranuleId`.

**Tech Stack:** Rust, Wasmtime runtime internals, `Arc`, `Mutex`/`RwLock`, existing `transaction` feature gate, existing `TxDurableLog`, `LockBased`, `TMemory`, `ObjectTable`, and file-backed block/chunk region infrastructure.

---

## Scope Boundary

This plan makes the prototype semantically thread-safe first, then improves concurrency granularity. The first working version may serialize transaction begin/commit/abort through one shared region mutex. That is acceptable because it proves the persistent identity and commit protocol are correct before performance work.

Do not base this work on WebAssembly Shared-Everything Threads. The runtime boundary is:

- local: `Store`, active workspace, live `VMGcRef`, live promotion maps, stack roots
- shared: `TransactionId`, `GranuleId`, `ObjectId`, durable roots, durable logs, block/chunk allocator, persistent object index, persistent GC coordinator

Raw live `VMGcRef`, `VMFuncRef`, or host `ExternRef` values must not be stored in shared transaction state.

## File Structure

- Create: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - Defines `TransactionRegionRuntime`, `TransactionRegionRuntimeInner`, shared lock/version/durable-log coordination, and test-only constructors.
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Exports the shared runtime behind the `transaction` feature.
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
  - Shrinks `TransactionState` toward local active transaction/workspace state and routes global operations through `TransactionRegionRuntime`.
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
  - Keeps `LockBased` as the algorithm, but makes it owned by the shared runtime rather than by each store-local `TransactionState`.
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Adds shared-runtime-safe durable log segment registration and recovery helpers while preserving independent per-thread/per-stream publication.
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
  - Separates live store-local bridge data from shared persistent object metadata.
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Routes persistent GC through the shared runtime's region-level GC coordinator.
- Modify: `crates/wasmtime/src/runtime/store.rs`
  - Adds an `Arc<TransactionRegionRuntime>` to store internals and test/runtime setup helpers for sharing it between stores.
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Passes transaction operations through store-local state plus shared region runtime.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs` and `crates/wasmtime/src/runtime/vm/memory/tmemory/*.rs`
  - Adds shared-region commit hooks for persistent `tmemory` where needed.
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Adds focused multi-thread and multi-store tests.
- Test: `tests/transaction_persistence.rs`
  - Adds file-backed recovery tests after concurrent committed and aborted transactions.

## Roadmap Overview

1. Add a shared runtime shell with a coarse mutex.
2. Move transaction id allocation and current-thread bookkeeping to the new boundary.
3. Move lock/version authority into shared region state.
4. Make durable publication thread-safe through per-thread/per-stream log segments, not a global publication lock.
5. Make persistent object roots and object index shared, while leaving live Wasmtime bridges store-local.
6. Route `tmemory` commit and recovery through the shared runtime.
7. Add a persistent GC coordinator with an exclusive region maintenance lock.
8. Add real multi-thread tests and then refine lock granularity.

---

### Task 1: Add Shared Region Runtime Shell

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write failing tests for store sharing**

Add tests that prove two stores can be configured to use the same transaction region runtime and that a default store still creates an isolated runtime.

```rust
#[test]
fn transaction_region_runtime_can_be_shared_between_stores_for_test() {
    let engine = crate::Engine::default();
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();

    let mut first = crate::Store::new(&engine, ());
    let mut second = crate::Store::new(&engine, ());

    first.set_transaction_region_runtime_for_test(runtime.clone());
    second.set_transaction_region_runtime_for_test(runtime.clone());

    assert!(first.transaction_region_runtime_is_same_for_test(&second));
}

#[test]
fn stores_have_distinct_transaction_region_runtimes_by_default() {
    let engine = crate::Engine::default();
    let first = crate::Store::new(&engine, ());
    let second = crate::Store::new(&engine, ());

    assert!(!first.transaction_region_runtime_is_same_for_test(&second));
}
```

- [ ] **Step 2: Run tests and verify compile failure**

Run:

```sh
cargo test -p wasmtime transaction_region_runtime_can_be_shared_between_stores_for_test stores_have_distinct_transaction_region_runtimes_by_default --lib -- --format terse
```

Expected: compile failure because `TransactionRegionRuntime` and the store test helpers do not exist.

- [ ] **Step 3: Add the shared runtime shell**

Create `region_runtime.rs`:

```rust
use crate::prelude::*;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{LockBased, TxDurableLog};

#[derive(Clone, Debug)]
pub(crate) struct TransactionRegionRuntime {
    inner: Arc<Mutex<TransactionRegionRuntimeInner>>,
}

#[derive(Debug)]
pub(crate) struct TransactionRegionRuntimeInner {
    pub(super) locks: LockBased,
    pub(super) durable_log: TxDurableLog,
}

impl Default for TransactionRegionRuntime {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TransactionRegionRuntimeInner {
                locks: LockBased::default(),
                durable_log: TxDurableLog::default(),
            })),
        }
    }
}

impl TransactionRegionRuntime {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::new()
    }

    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, TransactionRegionRuntimeInner>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("transaction region runtime mutex poisoned"))
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}
```

- [ ] **Step 4: Export the runtime**

Update `crates/wasmtime/src/runtime/transaction/mod.rs`:

```rust
mod region_runtime;

pub(crate) use region_runtime::{TransactionRegionRuntime, TransactionRegionRuntimeInner};
```

- [ ] **Step 5: Attach the runtime to `StoreOpaque`**

Add a field to `StoreOpaque` in `crates/wasmtime/src/runtime/store.rs`:

```rust
transaction_region_runtime: TransactionRegionRuntime,
```

Initialize it in store construction:

```rust
transaction_region_runtime: TransactionRegionRuntime::new(),
```

Add accessors:

```rust
pub(crate) fn transaction_region_runtime(&self) -> &TransactionRegionRuntime {
    &self.transaction_region_runtime
}

pub(crate) fn transaction_region_runtime_mut(&mut self) -> &mut TransactionRegionRuntime {
    &mut self.transaction_region_runtime
}
```

Add test helpers on `Store<T>`:

```rust
#[cfg(test)]
pub(crate) fn set_transaction_region_runtime_for_test(
    &mut self,
    runtime: TransactionRegionRuntime,
) {
    self.inner.transaction_region_runtime = runtime;
}

#[cfg(test)]
pub(crate) fn transaction_region_runtime_is_same_for_test<U>(
    &self,
    other: &Store<U>,
) -> bool {
    self.inner
        .transaction_region_runtime
        .ptr_eq_for_test(&other.inner.transaction_region_runtime)
}
```

- [ ] **Step 6: Run focused tests**

Run:

```sh
cargo test -p wasmtime transaction_region_runtime_can_be_shared_between_stores_for_test stores_have_distinct_transaction_region_runtimes_by_default --lib -- --format terse
```

Expected: both tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/mod.rs crates/wasmtime/src/runtime/store.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add shared transaction region runtime shell"
```

---

### Task 2: Move Transaction Id Allocation To Shared Runtime

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add failing tests for globally unique ids**

```rust
#[test]
fn shared_region_runtime_allocates_unique_transaction_ids_across_threads() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let barrier = Arc::new(Barrier::new(2));

    let first_runtime = runtime.clone();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_runtime.allocate_transaction_id_for_test().unwrap()
    });

    let second_runtime = runtime.clone();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_runtime.allocate_transaction_id_for_test().unwrap()
    });

    let first = first.join().unwrap();
    let second = second.join().unwrap();

    assert_ne!(first, second);
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime shared_region_runtime_allocates_unique_transaction_ids_across_threads --lib -- --format terse
```

Expected: compile failure because shared id allocation does not exist.

- [ ] **Step 3: Add shared id allocation**

Update `TransactionRegionRuntimeInner`:

```rust
pub(super) next_transaction_id: u64,
```

Initialize it:

```rust
next_transaction_id: 1,
```

Add runtime method:

```rust
pub(crate) fn allocate_transaction_id(&self) -> Result<TransactionId> {
    let mut inner = self.lock()?;
    let id = TransactionId::from_raw(inner.next_transaction_id);
    inner.next_transaction_id = inner
        .next_transaction_id
        .checked_add(1)
        .context("transaction id overflow")?;
    Ok(id)
}

#[cfg(test)]
pub(crate) fn allocate_transaction_id_for_test(&self) -> Result<TransactionId> {
    self.allocate_transaction_id()
}
```

- [ ] **Step 4: Route `TransactionState::begin` through shared allocation**

Add a new method:

```rust
pub(crate) fn begin_with_region_runtime(
    &mut self,
    region: &TransactionRegionRuntime,
) -> Result<TransactionId> {
    ensure!(self.active.is_none(), "transaction is already active");
    let id = region.allocate_transaction_id()?;
    self.active = Some(id);
    replace_current_thread_transaction(Some(id));
    self.workspaces.entry(id).or_default();
    Ok(id)
}
```

Keep the old `begin` as a test-only wrapper or route it through an isolated default runtime until all call sites are migrated.

- [ ] **Step 5: Update lifecycle libcalls**

In `crates/wasmtime/src/runtime/vm/libcalls.rs`, replace direct `state.begin()` for production paths with:

```rust
let region = store.store_opaque().transaction_region_runtime().clone();
let state = store.store_opaque_mut().transaction_state_mut();
let transaction = state.begin_with_region_runtime(&region)?;
```

- [ ] **Step 6: Run focused lifecycle tests**

Run:

```sh
cargo test -p wasmtime transaction:: --lib -- --format terse
```

Expected: transaction runtime tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Allocate transaction ids from shared region runtime"
```

---

### Task 3: Move LockBased And Version Authority To Shared Runtime

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add failing multi-thread conflict tests**

```rust
#[test]
fn shared_region_runtime_detects_cross_thread_write_conflict() {
    use crate::runtime::transaction::{GranuleId, TransactionId};
    use std::sync::{Arc, Barrier};
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let granule = GranuleId::TMemory { memory: 0, granule: 7 };
    let barrier = Arc::new(Barrier::new(2));

    let first_runtime = runtime.clone();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_runtime
            .acquire_granule_write_for_test(TransactionId::from_raw(1), granule, 0)
            .unwrap();
        first_barrier.wait();
        first_runtime.release_transaction_for_test(TransactionId::from_raw(1)).unwrap();
    });

    let second_runtime = runtime.clone();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_runtime
            .acquire_granule_write_for_test(TransactionId::from_raw(2), granule, 0)
            .is_err()
    });

    first.join().unwrap();
    assert!(second.join().unwrap());
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime shared_region_runtime_detects_cross_thread_write_conflict --lib -- --format terse
```

Expected: compile failure because runtime lock methods do not exist.

- [ ] **Step 3: Add shared lock methods**

Add to `TransactionRegionRuntime`:

```rust
pub(crate) fn acquire_granule_read(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<Option<TransactionId>> {
    self.lock()?
        .locks
        .record_read(transaction, granule, current_version)
}

pub(crate) fn acquire_granule_write(
    &self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<Option<TransactionId>> {
    self.lock()?
        .locks
        .acquire_write(transaction, granule, current_version)
}

pub(crate) fn release_transaction(&self, transaction: TransactionId) -> Result<()> {
    self.lock()?.locks.release_transaction(transaction);
    Ok(())
}
```

Add test wrappers with `_for_test` suffix.

- [ ] **Step 4: Route state acquisition through shared runtime**

For each `TransactionState` helper that currently calls `self.concurrency` or `self.lock_based`, pass a `&TransactionRegionRuntime` argument and call:

```rust
region.acquire_granule_read(transaction, granule, current_version)?;
region.acquire_granule_write(transaction, granule, current_version)?;
```

- [ ] **Step 5: Preserve abort cleanup**

When aborting or committing a transaction, release ownership through:

```rust
region.release_transaction(transaction)?;
replace_current_thread_transaction(None);
```

- [ ] **Step 6: Run conflict and WAST tests**

Run:

```sh
cargo test -p wasmtime lock_based shared_region_runtime_detects_cross_thread_write_conflict --lib -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tconflict-basic.wast -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tconflict-tmemory_1.wast -- --format terse
```

Expected: all focused tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/concurrency.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Share transaction lock state across stores"
```

---

### Task 4: Add Per-Thread Durable Log Segments

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Test: `tests/transaction_persistence.rs`

- [ ] **Step 1: Add failing per-thread segment registration test**

```rust
#[test]
fn shared_region_runtime_assigns_distinct_log_segments_to_threads() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let barrier = Arc::new(Barrier::new(2));

    let first_runtime = runtime.clone();
    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_runtime.current_thread_log_segment_for_test().unwrap()
    });

    let second_runtime = runtime.clone();
    let second_barrier = barrier.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_runtime.current_thread_log_segment_for_test().unwrap()
    });

    let first = first.join().unwrap();
    let second = second.join().unwrap();

    assert_ne!(first.stream_id(), second.stream_id());
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime shared_region_runtime_assigns_distinct_log_segments_to_threads --lib -- --format terse
```

Expected: compile failure because shared log segment assignment does not exist.

- [ ] **Step 3: Add log segment identity and assignment**

Add a small durable segment identity:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableLogSegment {
    stream_id: u32,
}

impl DurableLogSegment {
    pub(crate) fn stream_id(self) -> u32 {
        self.stream_id
    }
}
```

Add to `TransactionRegionRuntimeInner`:

```rust
next_log_segment_stream_id: u32,
thread_log_segments: BTreeMap<std::thread::ThreadId, DurableLogSegment>,
```

Add to `TransactionRegionRuntime`:

```rust
pub(crate) fn current_thread_log_segment(&self) -> Result<DurableLogSegment> {
    let thread_id = std::thread::current().id();
    let mut inner = self.lock()?;
    if let Some(segment) = inner.thread_log_segments.get(&thread_id).copied() {
        return Ok(segment);
    }
    let stream_id = inner.next_log_segment_stream_id;
    inner.next_log_segment_stream_id = inner
        .next_log_segment_stream_id
        .checked_add(1)
        .context("transaction log segment stream id overflow")?;
    let segment = DurableLogSegment { stream_id };
    inner.thread_log_segments.insert(thread_id, segment);
    Ok(segment)
}
```

- [ ] **Step 4: Route durable stream selection through the segment**

Production commit paths should derive the durable `stream_id` from the current thread's log segment when a shared runtime is attached:

```rust
let stream_id = if let Some(region) = state.shared_region_runtime() {
    region.current_thread_log_segment()?.stream_id()
} else {
    u32::try_from(transaction_id).context("transaction id does not fit durable transaction stream id")?
};
let txid = u32::try_from(transaction_id).context("transaction id does not fit durable transaction id")?;
```

Keep actual publication in the thread-local/store-local durable log segment for now. Do not route publication through a global runtime mutex. The durable backend may still briefly lock global allocation/free-list state when it needs a fresh block/chunk.

- [ ] **Step 5: Preserve ordering**

Keep the existing sequence inside `StreamPublisher`:

```text
append data record
flush data
fence
append log entry with LP cleared
flush log
fence
flip LP on final log entry
flush log
fence
```

Do not recalculate CRC after LP flip. CRC validation must keep checking with the LP bit cleared.

- [ ] **Step 6: Add a non-serialization publication test**

Add a test that publishes one committed tmemory undo from each of two thread-local durable logs using distinct stream ids assigned by the shared runtime. The test should assert:

- the two assigned stream ids differ
- each local log has exactly one committed entry for its assigned stream
- no global shared-runtime publication API is used

- [ ] **Step 7: Run durable-log tests**

Run:

```sh
cargo test -p wasmtime shared_region_runtime_assigns_distinct_log_segments_to_threads --lib -- --format terse
cargo test -p wasmtime transaction::persist --lib -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: all durable publication tests pass.

- [ ] **Step 8: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs tests/transaction_persistence.rs
git commit -m "Add per-thread transaction log segments"
```

---

### Task 5: Share Persistent Object Index And Roots

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/durable_ref.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add failing test for cross-store root visibility**

```rust
#[test]
fn committed_persistent_root_is_visible_to_another_store_using_same_region_runtime() {
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let object = crate::runtime::transaction::ObjectId::from_raw_parts_for_test(17);

    runtime
        .publish_root_for_test("bank", object, 1)
        .expect("publish root");

    assert_eq!(
        runtime.lookup_root_for_test("bank").expect("lookup root"),
        Some(object)
    );
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime committed_persistent_root_is_visible_to_another_store_using_same_region_runtime --lib -- --format terse
```

Expected: compile failure because root lookup/publication is not shared.

- [ ] **Step 3: Add shared persistent object metadata**

Add to `TransactionRegionRuntimeInner`:

```rust
pub(super) persistent_roots: BTreeMap<String, (ObjectId, u32)>,
pub(super) object_versions: BTreeMap<ObjectId, u32>,
```

Initialize both maps to `BTreeMap::new()`.

- [ ] **Step 4: Add root APIs**

```rust
pub(crate) fn publish_root(
    &self,
    name: &str,
    object: ObjectId,
    version: u32,
) -> Result<()> {
    self.lock()?
        .persistent_roots
        .insert(name.to_owned(), (object, version));
    Ok(())
}

pub(crate) fn lookup_root(&self, name: &str) -> Result<Option<ObjectId>> {
    Ok(self.lock()?.persistent_roots.get(name).map(|(object, _)| *object))
}
```

- [ ] **Step 5: Keep live bridges store-local**

Verify that these fields remain in `ObjectTable` and are not moved into `TransactionRegionRuntimeInner`:

```rust
live_bridge_gc_refs_to_objects
object_to_live_bridge_gc_ref
live_bridge_func_refs_to_objects
object_to_live_bridge_func_ref
transaction_ref_handles_to_objects
```

If any commit path needs these maps, it must consume them before publication and publish only `ObjectId`/durable scalar payloads.

- [ ] **Step 6: Run object recovery tests**

Run:

```sh
cargo test -p wasmtime persistent_root object_table rebuild_from_recovered --lib -- --format terse
```

Expected: root and object-table tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/object_table.rs crates/wasmtime/src/runtime/transaction/durable_ref.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Share persistent object roots through region runtime"
```

---

### Task 6: Route Persistent TMemory Through Shared Runtime

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `tests/transaction_persistence.rs`

- [ ] **Step 1: Add failing file-backed two-thread linear-memory test**

Add a test that opens one file-backed transaction region runtime, starts two host threads, writes different `tmemory` granules, commits both, reopens storage, and verifies both writes survived.

```rust
#[test]
fn file_backed_tmemory_commits_from_two_threads_recover_together() {
    let temp = tempfile::tempdir().unwrap();
    let tmemory_path = temp.path().join("tmemory.bin");
    let tx_log_path = temp.path().join("txlog.bin");

    let runtime = crate::runtime::transaction::TransactionRegionRuntime::create_file_backed_for_test(
        &tmemory_path,
        &tx_log_path,
        128,
    )
    .unwrap();

    run_two_thread_tmemory_commit_for_test(runtime.clone(), 0, 0x11);
    run_two_thread_tmemory_commit_for_test(runtime.clone(), 1, 0x22);

    let recovered = crate::runtime::transaction::TransactionRegionRuntime::open_file_backed_for_test(
        &tmemory_path,
        &tx_log_path,
    )
    .unwrap();

    assert_eq!(recovered.tmemory_byte_for_test(0).unwrap(), 0x11);
    assert_eq!(recovered.tmemory_byte_for_test(crate::runtime::transaction::TMEMORY_GRANULE_SIZE).unwrap(), 0x22);
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime --test transaction_persistence file_backed_tmemory_commits_from_two_threads_recover_together -- --format terse
```

Expected: compile failure because shared file-backed runtime helpers do not exist.

- [ ] **Step 3: Add shared runtime file-backed constructors**

Add methods that configure both persistent `tmemory` and durable log under one shared runtime:

```rust
pub(crate) fn create_file_backed_for_test(
    tmemory_path: &Path,
    tx_log_path: &Path,
    tx_log_blocks: u32,
) -> Result<Self> {
    let runtime = Self::new();
    {
        let mut inner = runtime.lock()?;
        inner.durable_log = TxDurableLog::create_file_backed(tx_log_path, tx_log_blocks)?;
        inner.file_backed_tmemory_path = Some(tmemory_path.to_owned());
    }
    Ok(runtime)
}
```

- [ ] **Step 4: Route commit-time undo publication through shared runtime**

Keep the existing linear-memory undo scheme, but make the shared runtime own the durable append and LP publication. The store-local `TMemory` may still stage bytes, but the final in-place write must happen after the shared runtime has published and fenced the undo record.

- [ ] **Step 5: Synchronize grow**

Protect `tmemory.grow` for file-backed/persistent backends with the shared region mutex. Growing must not interleave with another transaction's commit over the same memory.

- [ ] **Step 6: Run persistence tests**

Run:

```sh
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test --test transaction_persistence_wast -- --format terse
```

Expected: persistence tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/linear_region.rs crates/wasmtime/src/runtime/transaction/state.rs tests/transaction_persistence.rs
git commit -m "Route persistent tmemory commits through shared runtime"
```

---

### Task 7: Add Persistent GC Region Coordinator

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add failing test that GC excludes active commits**

```rust
#[test]
fn persistent_gc_excludes_user_transaction_commits() {
    let runtime = crate::runtime::transaction::TransactionRegionRuntime::new_for_test();
    let permit = runtime.begin_user_transaction_region_for_test().unwrap();

    let gc_result = runtime.begin_persistent_gc_for_test();
    assert!(gc_result.is_err());

    drop(permit);
    assert!(runtime.begin_persistent_gc_for_test().is_ok());
}
```

- [ ] **Step 2: Run test and verify failure**

Run:

```sh
cargo test -p wasmtime persistent_gc_excludes_user_transaction_commits --lib -- --format terse
```

Expected: compile failure because GC permits do not exist.

- [ ] **Step 3: Add GC state**

Add to `TransactionRegionRuntimeInner`:

```rust
pub(super) active_user_commits: u32,
pub(super) gc_active: bool,
```

Add permit types in `region_runtime.rs`:

```rust
pub(crate) struct UserTransactionRegionPermit {
    runtime: TransactionRegionRuntime,
}

pub(crate) struct PersistentGcRegionPermit {
    runtime: TransactionRegionRuntime,
}
```

- [ ] **Step 4: Implement permit acquisition**

```rust
pub(crate) fn begin_user_transaction_region(&self) -> Result<UserTransactionRegionPermit> {
    let mut inner = self.lock()?;
    ensure!(!inner.gc_active, "persistent GC is active");
    inner.active_user_commits = inner
        .active_user_commits
        .checked_add(1)
        .context("active transaction commit count overflow")?;
    Ok(UserTransactionRegionPermit { runtime: self.clone() })
}

pub(crate) fn begin_persistent_gc(&self) -> Result<PersistentGcRegionPermit> {
    let mut inner = self.lock()?;
    ensure!(!inner.gc_active, "persistent GC is already active");
    ensure!(inner.active_user_commits == 0, "user transaction commit is active");
    inner.gc_active = true;
    Ok(PersistentGcRegionPermit { runtime: self.clone() })
}
```

Implement `Drop` for both permits to release counters/state.

- [ ] **Step 5: Use permits in commit and GC paths**

At the start of commit publication:

```rust
let _permit = region.begin_user_transaction_region()?;
```

At the start of persistent GC:

```rust
let _permit = region.begin_persistent_gc()?;
```

- [ ] **Step 6: Run persistent GC tests**

Run:

```sh
cargo test -p wasmtime persistent_gc object_gc --lib -- --format terse
```

Expected: GC tests pass.

- [ ] **Step 7: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/object_gc.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Coordinate persistent GC with shared transaction runtime"
```

---

### Task 8: Add Multi-Store End-To-End Tests

**Files:**
- Test: `tests/transaction_persistence.rs`
- Test: `tests/transaction_persistence_wast.rs`
- Modify only if needed: `crates/test-util/src/wast.rs`

- [ ] **Step 1: Add two-store object commit/recovery test**

Create a test that:

1. Creates one file-backed shared region runtime.
2. Creates two stores using that runtime.
3. Commits object graph A from store A.
4. Commits object graph B from store B.
5. Reopens storage.
6. Verifies both durable roots and both object graphs recover.

- [ ] **Step 2: Add same-object conflict test**

Create a test that:

1. Creates one shared runtime.
2. Starts transaction A and writes object `ObjectId(X)`.
3. Starts transaction B and attempts to write `ObjectId(X)`.
4. Verifies one transaction aborts by `LockBased` conflict.
5. Verifies recovery contains only the committed version.

- [ ] **Step 3: Add mixed object plus tmemory test**

Create a test that:

1. In store A, commits a transaction touching `tmemory` granule 0 and object root `left`.
2. In store B, commits a transaction touching `tmemory` granule 1 and object root `right`.
3. Reopens file-backed storage.
4. Verifies both `tmemory` bytes and both roots recover.

- [ ] **Step 4: Run end-to-end tests**

Run:

```sh
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test --test transaction_persistence_wast -- --format terse
```

Expected: tests pass.

- [ ] **Step 5: Commit**

```sh
git add tests/transaction_persistence.rs tests/transaction_persistence_wast.rs crates/test-util/src/wast.rs
git commit -m "Add multi-store transaction persistence tests"
```

---

### Task 9: Reduce Lock Granularity After Correctness Is Proven

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add regression tests that prove disjoint commits can overlap**

Use test barriers and counters to show that two disjoint-granule transactions can both pass lock acquisition before either publishes LP.

- [ ] **Step 2: Split runtime state**

Replace the single `Mutex<TransactionRegionRuntimeInner>` with:

```rust
pub(crate) struct TransactionRegionRuntimeInner {
    next_transaction_id: AtomicU64,
    locks: Mutex<LockBased>,
    durable_log: Mutex<TxDurableLog>,
    persistent_roots: RwLock<BTreeMap<String, (ObjectId, u32)>>,
    object_versions: RwLock<BTreeMap<ObjectId, u32>>,
    gc_state: Mutex<PersistentGcState>,
}
```

- [ ] **Step 3: Keep commit ordering unchanged**

Durable publication must still serialize per durable log backend until the log backend has per-stream locking. Do not split log locks before all LP and recovery tests pass under the single log mutex.

- [ ] **Step 4: Run concurrency tests repeatedly**

Run:

```sh
for i in 1 2 3 4 5; do cargo test -p wasmtime transaction:: --lib -- --format terse || exit 1; done
```

Expected: all five runs pass.

- [ ] **Step 5: Commit**

```sh
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs crates/wasmtime/src/runtime/transaction/concurrency.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Split shared transaction runtime locks"
```

---

### Task 10: Final Verification Gate

**Files:**
- Modify only for fixes discovered by the gate.

- [ ] **Step 1: Run formatting and diff checks**

```sh
cargo fmt --check
git diff --check
```

Expected: both pass.

- [ ] **Step 2: Run focused transaction tests**

```sh
cargo test -p wasmtime transaction:: --lib -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test --test transaction_persistence_wast -- --format terse
cargo test -p wasmtime-transaction-sdk --tests -- --format terse
cargo test -p wasmtime-transaction-tools --tests -- --format terse
```

Expected: all pass.

- [ ] **Step 3: Run proposal WAST tests**

```sh
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

Expected: all enabled transaction proposal tests pass with `0 ignored`.

- [ ] **Step 4: Run full transaction-enabled gate if machine resources permit**

```sh
CARGO_INCREMENTAL=0 WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

Expected: pass.

- [ ] **Step 5: Document final status**

Update `docs/shisoft/transactional-wasm-implementation-log.md` with:

```markdown
## YYYY-MM-DD: Shared Transaction Region Runtime

- Added `TransactionRegionRuntime` as the shared authority for multi-store transactions.
- Store-local state now keeps active workspaces and live Wasmtime bridge refs only.
- Shared state owns transaction id allocation, granule locks, versions, durable publication, persistent roots, and persistent GC coordination.
- Multi-store file-backed transaction persistence tests pass for linear memory, persistent objects, and mixed transactions.
```

- [ ] **Step 6: Commit**

```sh
git add docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document shared transaction runtime status"
```

---

## Success Criteria

- Multiple stores can share one `TransactionRegionRuntime`.
- Transaction ids are globally unique within a shared runtime.
- Lock conflicts are detected across host threads.
- Durable publication uses the same data/log/LP ordering as the current single-thread path.
- `VMGcRef` and `VMFuncRef` remain store-local bridge state and never become persistent shared identifiers.
- `ObjectId` and `GranuleId` are the only shared object/granule identities.
- File-backed recovery works after concurrent commits and loose ends.
- Persistent GC excludes user commits and user commits exclude persistent GC.
- Existing WAST and transaction persistence tests continue to pass.

## Self-Review

- Spec coverage: the plan moves store-local transaction authority into a shared runtime, keeps live GC refs local, preserves durable publication ordering, and adds multi-thread tests for locks, logs, tmemory, objects, and GC.
- Placeholder scan: the roadmap intentionally avoids unspecified work items; each task names files, test intent, and verification commands.
- Type consistency: the shared boundary consistently uses `TransactionRegionRuntime`, `TransactionId`, `GranuleId`, `ObjectId`, `PendingGranuleUndo`, `PendingPublication`, `PendingCommitLogEntry`, and `TxDurableLog`.
