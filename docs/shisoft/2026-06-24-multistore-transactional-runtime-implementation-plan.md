# Multi-Store Transactional Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make many Wasmtime stores behave like VM threads attached to one shared transactional persistent heap, with shared linear memory, shared persistent object authority, transaction isolation, and store-local execution/workspace state.

**Architecture:** Keep `Store<T>` as the per-thread execution container, but move authoritative persistent object state into the shared `TransactionRegionRuntime`. The store-local `ObjectTable` remains a cache, handle table, type-layout view, and transaction workspace helper; persistent object reads validate/materialize through the shared runtime before dereference.

**Tech Stack:** Rust, Wasmtime runtime internals, `Arc<Mutex<_>>` / `Arc<RwLock<_>>`, existing transaction modules, existing DAX/file-backed durable region log, `cargo test -p wasmtime`.

---

## Scope

This plan implements the behavior model discussed in the design conversation:

- A thread/task owns a `Store`.
- A shared `TransactionRegionRuntime` owns persistent object identity, latest object record metadata, persistent roots, concurrency-control authority, durable-region configuration, and persistent GC coordination.
- Store-local state owns transaction staging, local roots, transient handles, object cache entries, live ref bridge metadata, and Wasm execution state.
- Shared linear memory remains directly addressable through the backend mapping. This plan does not make Wasmtime's ordinary non-shared `Memory`, `Table`, `Global`, or GC heap globally shared.

Non-goals:

- Do not make `Store<T>` internally synchronized or globally shareable.
- Do not implement MVCC.
- Do not change the log typing.
- Do not add per-object routing metadata to durable log entries.
- Do not use `ObjectId` to choose a NUMA region. Object placement remains determined by allocation address/region.

## File Structure

- Create `crates/wasmtime/src/runtime/transaction/object_directory.rs`
  - Shared persistent object directory entry types.
  - Conversion helpers from committed publication metadata to shared directory records.
  - Unit tests for version ordering and stale-entry rejection.

- Modify `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Register the new module.
  - Re-export crate-private directory types needed by `region_runtime`, `object_table`, `state`, and tests.

- Modify `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - Add shared persistent object directory storage to `TransactionRegionRuntimeInner`.
  - Add object version, install, lookup, and recovery-adoption APIs.
  - Make shared runtime object versions available to concurrency validation.

- Modify `crates/wasmtime/src/runtime/transaction/object_table.rs`
  - Keep local slots as cache entries.
  - Add APIs that install/materialize a persistent object slot from a shared directory entry.
  - Add APIs that refresh a persistent object before payload/type/version access.
  - Keep transient object allocations and live ref bridge state store-local.

- Modify `crates/wasmtime/src/runtime/transaction/state.rs`
  - Use shared object versions for `GranuleId::Object` when a shared runtime exists.
  - Register committed object publications with shared runtime after LP and before releasing commit authority.
  - Refresh object cache before acquire/read/write/validation paths.

- Modify `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Keep `GranuleId::Object` on the object-specific version path and use shared directory versions instead of store-local object-table versions when a shared runtime is attached.

- Modify `crates/wasmtime/src/runtime/store.rs`
  - Add explicit helpers to attach/adopt an existing transaction runtime into a store.
  - Ensure open/recovery paths seed both shared runtime metadata and the local object cache.

- Modify `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Route transactional object dereference helpers through refresh-aware object table/state methods.

- Modify `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Add focused unit tests for directory behavior.
  - Add multi-store behavioral tests for object allocation, read/write conflict, latest-version visibility, and persistent recovery adoption.

---

## Task 1: Add Shared Persistent Object Directory Types

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/object_directory.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Create the directory data model**

Add `object_directory.rs` with these crate-private types:

```rust
use crate::prelude::*;
use alloc::sync::Arc;
use core::fmt;
use crate::runtime::transaction::{ObjectId, ObjectKind};
use crate::runtime::vm::block_region::MappedRegionSource;
use wasmtime_environ::VMSharedTypeIndex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectRecordLocation {
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) data_record_offset: u64,
    pub(crate) record_len: u64,
}

#[derive(Clone)]
pub(crate) struct PersistentObjectRecordSource {
    pub(crate) mapped_source: Arc<dyn MappedRegionSource>,
    pub(crate) location: PersistentObjectRecordLocation,
}

impl fmt::Debug for PersistentObjectRecordSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistentObjectRecordSource")
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PersistentObjectDirectoryEntry {
    pub(crate) object_id: ObjectId,
    pub(crate) kind: ObjectKind,
    pub(crate) directory_version: u64,
    pub(crate) record_version: u32,
    pub(crate) type_layout_id: u32,
    pub(crate) runtime_type_index: Option<VMSharedTypeIndex>,
    pub(crate) record_source: Option<PersistentObjectRecordSource>,
}

impl PersistentObjectDirectoryEntry {
    pub(crate) fn ensure_matches_object(self, object_id: ObjectId) -> Result<Self> {
        ensure!(
            self.object_id == object_id,
            "persistent object directory entry object id mismatch"
        );
        Ok(self)
    }
}
```

- [ ] **Step 2: Register the module**

In `crates/wasmtime/src/runtime/transaction/mod.rs`, add:

```rust
mod object_directory;
pub(crate) use object_directory::{
    PersistentObjectDirectoryEntry, PersistentObjectRecordLocation, PersistentObjectRecordSource,
};
```

- [ ] **Step 3: Add first directory tests**

Add tests:

```rust
#[test]
fn persistent_object_directory_entry_rejects_wrong_object_id() {
    let entry = PersistentObjectDirectoryEntry {
        object_id: ObjectId { object_index: 7 },
        kind: ObjectKind::Struct,
        directory_version: 3,
        record_version: 2,
        type_layout_id: 1,
        runtime_type_index: None,
        record_source: None,
    };

    assert!(
        entry
            .ensure_matches_object(ObjectId { object_index: 8 })
            .is_err()
    );
}
```

- [ ] **Step 4: Run the focused test**

Run:

```bash
cargo test -p wasmtime persistent_object_directory_entry_rejects_wrong_object_id --lib
```

Expected: the new test passes.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_directory.rs \
        crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add persistent object directory entry types"
```

---

## Task 2: Add Shared Directory Authority To TransactionRegionRuntime

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add directory state to the shared runtime**

Add imports:

```rust
use super::{PersistentObjectDirectoryEntry, PersistentObjectRecordLocation, PersistentObjectRecordSource};
```

Extend `TransactionRegionRuntimeInner`:

```rust
object_directory: Mutex<PersistentObjectDirectoryState>,
```

Add the state type:

```rust
#[derive(Debug, Default)]
struct PersistentObjectDirectoryState {
    entries: BTreeMap<ObjectId, PersistentObjectDirectoryEntry>,
    next_directory_version: u64,
}
```

Initialize it in `Default for TransactionRegionRuntimeInner`:

```rust
object_directory: Mutex::new(PersistentObjectDirectoryState::default()),
```

- [ ] **Step 2: Add locking helper**

Add:

```rust
fn lock_object_directory(&self) -> Result<MutexGuard<'_, PersistentObjectDirectoryState>> {
    self.0
        .object_directory
        .lock()
        .map_err(|_| crate::format_err!("persistent object directory lock poisoned"))
}
```

- [ ] **Step 3: Add install and lookup APIs**

Add these methods on `TransactionRegionRuntime`:

```rust
pub(crate) fn install_persistent_object_directory_entries<I>(
    &self,
    entries: I,
) -> Result<Vec<PersistentObjectDirectoryEntry>>
where
    I: IntoIterator<Item = PersistentObjectDirectoryEntry>,
{
    let mut directory = self.lock_object_directory()?;
    let mut installed = Vec::new();
    for mut entry in entries {
        match directory.entries.get(&entry.object_id) {
            Some(current) if current.record_version > entry.record_version => continue,
            Some(current) if current.record_version == entry.record_version => {
                installed.push(current.clone());
                continue;
            }
            _ => {}
        }

        let next_version = directory
            .next_directory_version
            .checked_add(1)
            .context("persistent object directory version overflow")?;
        directory.next_directory_version = next_version;
        entry.directory_version = next_version;

        directory.entries.insert(entry.object_id, entry.clone());
        installed.push(entry);
    }
    Ok(installed)
}

pub(crate) fn persistent_object_directory_entry(
    &self,
    object_id: ObjectId,
) -> Result<Option<PersistentObjectDirectoryEntry>> {
    Ok(self.lock_object_directory()?.entries.get(&object_id).cloned())
}

pub(crate) fn persistent_object_directory_version(&self, object_id: ObjectId) -> Result<u64> {
    Ok(self
        .lock_object_directory()?
        .entries
        .get(&object_id)
        .map(|entry| entry.directory_version)
        .unwrap_or(0))
}
```

- [ ] **Step 4: Add tests for latest-entry behavior**

Add:

```rust
#[test]
fn shared_object_directory_keeps_highest_record_version() {
    let runtime = TransactionRegionRuntime::default();
    let object = ObjectId { object_index: 11 };

    runtime
        .install_persistent_object_directory_entries([PersistentObjectDirectoryEntry {
            object_id: object,
            kind: ObjectKind::Struct,
            directory_version: 0,
            record_version: 2,
            type_layout_id: 1,
            runtime_type_index: None,
            record_source: None,
        }])
        .unwrap();

    runtime
        .install_persistent_object_directory_entries([PersistentObjectDirectoryEntry {
            object_id: object,
            kind: ObjectKind::Struct,
            directory_version: 0,
            record_version: 1,
            type_layout_id: 1,
            runtime_type_index: None,
            record_source: None,
        }])
        .unwrap();

    let entry = runtime
        .persistent_object_directory_entry(object)
        .unwrap()
        .unwrap();
    assert_eq!(entry.record_version, 2);
    assert!(entry.directory_version > 0);
}
```

- [ ] **Step 5: Run the focused test**

Run:

```bash
cargo test -p wasmtime shared_object_directory_keeps_highest_record_version --lib
```

Expected: the new test passes.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add shared persistent object directory authority"
```

---

## Task 3: Use Shared Directory Versions For Object Granules

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Keep objects on the object-specific version path**

Leave `granule_uses_transaction_state_version` excluding `GranuleId::Object`. Object records are copy-on-write and publish through the shared directory; adding them to the generic `granule_versions` bump path would double-bump object write versions after commit.

```rust
fn granule_uses_transaction_state_version(granule: GranuleId) -> bool {
    matches!(
        granule,
        GranuleId::TMemorySize { .. }
            | GranuleId::TGlobal { .. }
            | GranuleId::TTable { .. }
            | GranuleId::TTableSize { .. }
    )
}
```

- [ ] **Step 2: Add current-version helper for object granules**

In `TransactionState`, add:

```rust
fn current_object_version_for_granule(
    &self,
    granule: GranuleId,
    object_table: &ObjectTable,
) -> Result<u64> {
    let Some(object_id) = object_granule_object_id(granule) else {
        return Ok(0);
    };
    if let Some(runtime) = &self.shared_region_runtime {
        return runtime.persistent_object_directory_version(object_id);
    }
    object_table.version(object_id)
}
```

- [ ] **Step 3: Use the shared version in object read validation**

Change `validate_active_object_reads`:

```rust
pub(crate) fn validate_active_object_reads(&self, object_table: &ObjectTable) -> Result<()> {
    for granule in self.active_read_granules()? {
        if object_granule_object_id(granule).is_none() {
            continue;
        }
        let version = self.current_object_version_for_granule(granule, object_table)?;
        self.validate_active_read(granule, version)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Add shared-version test**

Add:

```rust
#[test]
fn shared_runtime_object_directory_version_is_authoritative() {
    let runtime = TransactionRegionRuntime::default();
    let object = ObjectId { object_index: 19 };

    runtime
        .install_persistent_object_directory_entries([PersistentObjectDirectoryEntry {
            object_id: object,
            kind: ObjectKind::Struct,
            directory_version: 0,
            record_version: 1,
            type_layout_id: 1,
            runtime_type_index: None,
            record_source: None,
        }])
        .unwrap();

    let version = runtime.persistent_object_directory_version(object).unwrap();
    assert!(version > 0);
}
```

- [ ] **Step 5: Run tests**

Run:

```bash
cargo test -p wasmtime shared_runtime_object_directory_version_is_authoritative --lib
cargo test -p wasmtime transaction_object_read_validation_uses_object_table_versions --lib
```

Expected: new shared-version test passes; existing local-version test still passes for stores without shared runtime.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/mod.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Use shared versions for persistent object granules"
```

---

## Task 4: Materialize Store-Local Object Cache From Shared Directory

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add a refresh API to `ObjectTable`**

Add:

```rust
pub(crate) fn refresh_persistent_object_from_shared_directory(
    &mut self,
    object_id: ObjectId,
) -> Result<bool> {
    let Some(runtime) = &self.shared_region_runtime else {
        return Ok(false);
    };
    let Some(entry) = runtime.persistent_object_directory_entry(object_id)? else {
        return Ok(false);
    };

    if let Ok(slot) = self.live_slot(object_id)
        && slot.persistent
        && slot.version >= entry.directory_version
    {
        return Ok(false);
    }

    self.install_persistent_object_directory_entry(entry)?;
    Ok(true)
}
```

- [ ] **Step 2: Add the entry installation helper**

Add:

```rust
pub(crate) fn install_persistent_object_directory_entry(
    &mut self,
    entry: PersistentObjectDirectoryEntry,
) -> Result<()> {
    let index = object_slot_index(entry.object_id)?;
    if index >= self.slots.len() {
        self.slots.resize_with(index + 1, || None);
    }

    let record = match &entry.record_source {
        Some(source) => self.heap.install_mapped_persistent_record_from_source(
            entry.object_id,
            entry.kind,
            entry.record_version,
            entry.type_layout_id,
            &source.mapped_source,
            &source.location,
        )?,
        None => {
            bail!("persistent object directory entry has no mapped record source")
        }
    };

    let was_live = self.slots[index].is_some();
    self.slots[index] = Some(ObjectTableSlot {
        kind: entry.kind,
        version: entry.directory_version,
        type_layout_id: entry.type_layout_id,
        runtime_type_index: entry.runtime_type_index,
        persistent: true,
        current_record: record,
    });
    if !was_live {
        self.live_count = self
            .live_count
            .checked_add(1)
            .context("object table live count overflow")?;
    }
    Ok(())
}
```

The helper above requires a small `ObjectHeap` method that installs a mapped persistent record from `PersistentObjectRecordSource`. Implement it by reusing the existing mapped-record path currently used by `install_committed_mapped_persistent_publication`.

- [ ] **Step 3: Refresh before payload and version access**

Add mutable refresh-aware variants:

```rust
pub(crate) fn refreshed_version(&mut self, object_id: ObjectId) -> Result<u64> {
    self.refresh_persistent_object_from_shared_directory(object_id)?;
    self.version(object_id)
}

pub(crate) fn refreshed_payload(&mut self, object_id: ObjectId) -> Result<ObjectPayload> {
    self.refresh_persistent_object_from_shared_directory(object_id)?;
    self.payload(object_id)
}
```

- [ ] **Step 4: Add cache materialization test**

Add a test that:

1. Creates a `TransactionRegionRuntime`.
2. Creates one `ObjectTable` with the runtime attached.
3. Installs a persistent object into the directory with a mapped record source produced by an existing test fixture durable log.
4. Calls `refresh_persistent_object_from_shared_directory`.
5. Asserts the local table now has a live persistent slot for that `ObjectId`.

Use the existing object publication/recovery helpers already used by:

```bash
rg -n "install_committed_mapped_object_publications|recovered_object_table_installs_mapped_records" crates/wasmtime/src/runtime/transaction/tests.rs
```

Name the test:

```rust
store_local_object_table_materializes_shared_persistent_object_entry
```

- [ ] **Step 5: Run the focused test**

Run:

```bash
cargo test -p wasmtime store_local_object_table_materializes_shared_persistent_object_entry --lib
```

Expected: the test passes and the installed record is mapped, not copied into DRAM.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_table.rs \
        crates/wasmtime/src/runtime/transaction/object_heap.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Materialize persistent object cache from shared directory"
```

---

## Task 5: Refresh Persistent Objects On Transactional Dereference

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Refresh before object read acquisition**

Change `acquire_object_read`:

```rust
pub(crate) fn acquire_object_read(
    &mut self,
    object_table: &mut ObjectTable,
    object_id: ObjectId,
) -> Result<bool> {
    object_table.refresh_persistent_object_from_shared_directory(object_id)?;
    let version = self.current_object_version_for_granule(
        object_table.granule_id(object_id)?,
        object_table,
    )?;
    let acquired = self.acquire_granule_read(object_table.granule_id(object_id)?, version)?;
    self.drain_conflict_aborted_allocated_objects(object_table)?;
    Ok(acquired)
}
```

- [ ] **Step 2: Refresh before object write acquisition**

Change `acquire_object_write` with the same refresh and shared version lookup:

```rust
pub(crate) fn acquire_object_write(
    &mut self,
    object_table: &mut ObjectTable,
    object_id: ObjectId,
) -> Result<bool> {
    object_table.refresh_persistent_object_from_shared_directory(object_id)?;
    let granule = object_table.granule_id(object_id)?;
    let version = self.current_object_version_for_granule(granule, object_table)?;
    let acquired = self.acquire_granule_write(granule, version)?;
    self.drain_conflict_aborted_allocated_objects(object_table)?;
    Ok(acquired)
}
```

- [ ] **Step 3: Refresh before payload reads**

Change `read_object_payload`:

```rust
pub(crate) fn read_object_payload(
    &mut self,
    object_table: &mut ObjectTable,
    object_id: ObjectId,
) -> Result<ObjectPayload> {
    object_table.refresh_persistent_object_from_shared_directory(object_id)?;
    if object_table.is_persistent(object_id).unwrap_or(false) {
        let granule = object_table.granule_id(object_id)?;
        self.acquire_granule_read(granule, self.current_object_version_for_granule(granule, object_table)?)?;
    }
    if let Some(record) = self.staged_objects.get(&object_id) {
        return Ok(record.payload().clone());
    }
    object_table.refreshed_payload(object_id)
}
```

Then update callers in `state.rs` that pass `&ObjectTable` into read helpers so they pass `&mut ObjectTable`. Update libcall split-borrows by using existing `transaction_state_and_object_table_mut` helpers.

- [ ] **Step 4: Add stale-cache dereference test**

Add:

```rust
#[test]
fn second_store_deref_refreshes_stale_persistent_object_cache() {
    // Build one shared TransactionRegionRuntime.
    // Store/table A publishes object version 1.
    // Store/table B materializes version 1.
    // Store/table A publishes object version 2.
    // Store/table B reads through TransactionState::read_object_payload.
    // Assert B observes version 2 payload without reopening storage.
}
```

Use existing helper patterns around `staged_persistent_object_publication_does_not_replace_live_mapped_record` and `install_committed_mapped_object_publications`.

- [ ] **Step 5: Run the focused test**

Run:

```bash
cargo test -p wasmtime second_store_deref_refreshes_stale_persistent_object_cache --lib
```

Expected: the second store reads the latest committed payload after refresh.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/vm/libcalls.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Refresh persistent object cache on dereference"
```

---

## Task 6: Publish Object Directory Entries At Commit

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Build directory entries from committed mapped publications**

Add a return type:

```rust
#[derive(Clone, Debug)]
pub(crate) struct InstalledPersistentObjectPublication {
    pub(crate) object_id: ObjectId,
    pub(crate) directory_entry: PersistentObjectDirectoryEntry,
}
```

Add a validation helper that produces candidate shared directory entries from committed publications, markers, and the mapped durable source:

```rust
pub(crate) fn committed_mapped_persistent_publication_entries(
    &self,
    publications: &[persist::PendingPublication],
    markers: &[persist::PendingCommitLogEntry],
    mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
) -> Result<Vec<InstalledPersistentObjectPublication>>
```

Each validated publication should produce an entry with `directory_version: 0`; the shared runtime assigns the final directory version:

```rust
PersistentObjectDirectoryEntry {
    object_id,
    kind,
    directory_version: 0,
    record_version: publication.version,
    type_layout_id: object_header.type_layout_id,
    runtime_type_index: slot.runtime_type_index,
    record_source: Some(PersistentObjectRecordSource {
        mapped_source: mapped_source.clone(),
        location: PersistentObjectRecordLocation {
            data_block: marker.data_block,
            data_offset: marker.data_offset,
            data_record_offset,
            record_len: object_header.record_len,
        },
    }),
}
```

- [ ] **Step 2: Register entries in shared runtime after LP and materialize local cache**

In `TransactionState::install_committed_mapped_object_publications`, after LP:

```rust
let candidates = object_table.committed_mapped_persistent_publication_entries(
    publications,
    markers,
    mapped_source.clone(),
)?;

let assigned_entries = match &self.shared_region_runtime {
    Some(runtime) => runtime.install_persistent_object_directory_entries(
        candidates.iter().map(|installed| installed.directory_entry.clone()),
    )?,
    None => candidates
        .iter()
        .map(|installed| installed.directory_entry.clone())
        .collect(),
};

for entry in assigned_entries {
    object_table.install_persistent_object_directory_entry(entry)?;
}

Ok(candidates.into_iter().map(|installed| installed.object_id).collect())
```

- [ ] **Step 3: Preserve serialized fallback behavior**

When there is no mapped source, keep the existing serialized install path. Do not register a shared directory entry without a mapped record source; shared multi-store dereference requires mapped durable bytes.

- [ ] **Step 4: Add commit publication test**

Add:

```rust
#[test]
fn committed_mapped_object_publication_updates_shared_directory() {
    // Create shared runtime and attach it to object table/state.
    // Publish and commit one persistent object record.
    // Call install_committed_mapped_object_publications.
    // Assert runtime.persistent_object_directory_entry(object_id) exists.
    // Assert entry.record_source.is_some().
}
```

- [ ] **Step 5: Run the focused test**

Run:

```bash
cargo test -p wasmtime committed_mapped_object_publication_updates_shared_directory --lib
```

Expected: the shared runtime has a latest entry for the committed object.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_table.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Publish committed persistent objects to shared directory"
```

---

## Task 7: Adopt Shared Runtime Across Stores

**Files:**
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add store attach helper**

Add a crate-private helper:

```rust
#[cfg(feature = "transaction")]
pub(crate) fn transaction_attach_region_runtime_for_test(
    &mut self,
    runtime: TransactionRegionRuntime,
    config: TransactionConfig,
) -> Result<()> {
    self.transaction_region_runtime = runtime.clone();
    self.transaction_state
        .set_shared_region_runtime(Some(runtime.clone()));
    self.transaction_object_table
        .set_shared_region_runtime(Some(runtime));
    self.transaction_config = config;
    Ok(())
}
```

If a production-facing helper is needed after the tests pass, add a non-test API that takes a validated `TransactionRuntimeHandle`. Keep the first implementation crate-private.

- [ ] **Step 2: Update file-backed open path**

When `transaction_open_file_backed_storage_for_test` rebuilds recovered object winners, also install recovered directory entries into `self.transaction_region_runtime`. Use the same winner metadata that rebuilds the local object table:

```rust
self.transaction_region_runtime
    .install_recovered_persistent_object_directory_entries(&object_winners)?;
```

Implement the runtime helper using `RecoveredObjectWinner` and mapped source metadata.

- [ ] **Step 3: Add two-store attach test**

Add:

```rust
#[test]
fn two_stores_attach_to_same_transaction_region_runtime() {
    let runtime = TransactionRegionRuntime::default();
    let config = TransactionConfig::default();

    let engine = Engine::default();
    let mut store_a = Store::new(&engine, ());
    let mut store_b = Store::new(&engine, ());

    store_a
        .transaction_attach_region_runtime_for_test(runtime.clone(), config.clone())
        .unwrap();
    store_b
        .transaction_attach_region_runtime_for_test(runtime, config)
        .unwrap();

    assert!(store_a.transaction_region_runtime().is_shared_with(store_b.transaction_region_runtime()));
}
```

Add `TransactionRegionRuntime::is_shared_with(&self, other: &Self) -> bool` implemented with `Arc::ptr_eq`.

- [ ] **Step 4: Run the focused test**

Run:

```bash
cargo test -p wasmtime two_stores_attach_to_same_transaction_region_runtime --lib
```

Expected: both stores report the same runtime authority.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/store.rs \
        crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Allow stores to attach to shared transaction runtime"
```

---

## Task 8: Keep Persistent Allocation Identity Shared

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Confirm persistent allocations use runtime object IDs**

Keep the existing `allocate_persistent_object_id_at_least` path in `ObjectTable::allocate_payload_with_type_layout_id`. Add a test that two object tables attached to the same runtime cannot allocate the same persistent object id.

Test name:

```rust
shared_runtime_allocates_unique_persistent_object_ids_across_object_tables
```

- [ ] **Step 2: Record initial persistent allocation in the shared directory**

For persistent allocations that are not durable yet, register only the object-id reservation and type/kind metadata in a separate reservation map, not as latest committed directory entries:

```rust
#[derive(Debug, Default)]
struct PersistentObjectDirectoryState {
    entries: BTreeMap<ObjectId, PersistentObjectDirectoryEntry>,
    reservations: BTreeMap<ObjectId, PersistentObjectReservation>,
    next_directory_version: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersistentObjectReservation {
    kind: ObjectKind,
    type_layout_id: u32,
}
```

Do not allow dereference from another store until a committed directory entry exists.

- [ ] **Step 3: Add reservation conflict test**

Add:

```rust
#[test]
fn shared_runtime_rejects_duplicate_persistent_object_reservation() {
    let runtime = TransactionRegionRuntime::default();
    let object = ObjectId { object_index: 41 };
    runtime
        .reserve_persistent_object_metadata(object, ObjectKind::Struct, 1)
        .unwrap();
    assert!(
        runtime
            .reserve_persistent_object_metadata(object, ObjectKind::Array, 1)
            .is_err()
    );
}
```

- [ ] **Step 4: Run tests**

Run:

```bash
cargo test -p wasmtime shared_runtime_allocates_unique_persistent_object_ids_across_object_tables --lib
cargo test -p wasmtime shared_runtime_rejects_duplicate_persistent_object_reservation --lib
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_table.rs \
        crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Share persistent object allocation identity"
```

---

## Task 9: Multi-Store Object Conflict Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add same-object write conflict test**

Add:

```rust
#[test]
fn two_stores_conflict_on_same_persistent_object_write() {
    // Create shared runtime, two states, two object tables.
    // Attach both tables/states to the same runtime.
    // Create one committed persistent object entry.
    // Store A begins and acquires object write.
    // Store B begins and attempts object write on same object.
    // Assert the selected concurrency control reports the configured conflict behavior.
}
```

Use the current default concurrency control expectation from nearby lock-based conflict tests. If the default is no-wait abort, assert that store B receives a conflict-abort result rather than blocking.

- [ ] **Step 2: Add latest-read-after-commit test**

Add:

```rust
#[test]
fn second_store_reads_latest_persistent_object_after_first_store_commits() {
    // Store A commits a persistent object payload.
    // Store B has an older cached slot.
    // Store B begins a new transaction and reads the object.
    // Assert the read sees A's committed payload through shared directory refresh.
}
```

- [ ] **Step 3: Run tests**

Run:

```bash
cargo test -p wasmtime two_stores_conflict_on_same_persistent_object_write --lib
cargo test -p wasmtime second_store_reads_latest_persistent_object_after_first_store_commits --lib
```

Expected: conflict behavior is coordinated by the shared runtime, and latest committed object records are visible across stores.

- [ ] **Step 4: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Test multi-store persistent object concurrency"
```

---

## Task 10: Recovery Seeds Shared Object Directory

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add recovered winner installation API**

Add:

```rust
pub(crate) fn install_recovered_persistent_object_directory_entries(
    &self,
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
    mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
) -> Result<()> {
    let entries = winners
        .iter()
        .map(|winner| {
            let kind = object_kind_from_u16(winner.kind)?;
            Ok(PersistentObjectDirectoryEntry {
                object_id: ObjectId {
                    object_index: winner.object_id,
                },
                kind,
                directory_version: 0,
                record_version: winner.version,
                type_layout_id: winner.type_layout_id,
                runtime_type_index: None,
                record_source: Some(PersistentObjectRecordSource {
                    mapped_source: mapped_source.clone(),
                    location: PersistentObjectRecordLocation {
                        data_block: winner.data_block,
                        data_offset: winner.data_offset,
                        data_record_offset: winner.data_record_offset,
                        record_len: winner.record_len,
                    },
                }),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    self.install_persistent_object_directory_entries(entries).map(|_| ())
}
```

- [ ] **Step 2: Call it during open/recovery**

In `StoreOpaque::transaction_open_file_backed_storage_for_test`, after `object_winners` is computed:

```rust
self.transaction_region_runtime
    .install_recovered_persistent_object_directory_entries(&object_winners, mapped_source.clone())?;
```

Apply the same pattern for DAX/multi-region recovery helpers that produce `committed_object_winners`.

- [ ] **Step 3: Add recovery adoption test**

Add:

```rust
#[test]
fn recovered_shared_runtime_allows_fresh_store_to_materialize_object_without_recopy() {
    // Recover durable storage into shared runtime.
    // Attach a fresh store/object table.
    // Refresh one recovered object from the shared directory.
    // Assert current_record_is_persistent_mapped_for_test returns true.
}
```

- [ ] **Step 4: Run focused test**

Run:

```bash
cargo test -p wasmtime recovered_shared_runtime_allows_fresh_store_to_materialize_object_without_recopy --lib
```

Expected: fresh store materializes mapped persistent records from shared recovery metadata.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/region_runtime.rs \
        crates/wasmtime/src/runtime/store.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Seed shared object directory during recovery"
```

---

## Task 11: Persistent GC Reads Shared Latest State

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Refresh objects before GC graph traversal**

Before persistent GC traces an object from a local object table, call:

```rust
objects.refresh_persistent_object_from_shared_directory(object_id)?;
```

Keep the existing shared GC permit in `TransactionState::persistent_mark_sweep_collect`, `persistent_gc_maintenance_step`, and `finish_persistent_gc_cycle_and_sweep`.

- [ ] **Step 2: Keep GC single-coordinator for this implementation**

Document in code comments near the GC permit:

```rust
// Shared persistent GC remains single-coordinator: all stores observe the same
// directory, but one store drives a GC cycle while user commits are excluded by
// the region runtime GC permit.
```

- [ ] **Step 3: Add GC freshness test**

Add:

```rust
#[test]
fn persistent_gc_marks_latest_shared_object_records() {
    // Store A commits an object graph update.
    // Store B has stale cached object records.
    // Store B runs persistent GC mark.
    // Assert the mark includes the latest reachable object graph.
}
```

- [ ] **Step 4: Run focused test**

Run:

```bash
cargo test -p wasmtime persistent_gc_marks_latest_shared_object_records --lib
```

Expected: GC tracing sees the latest directory-backed graph.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_gc.rs \
        crates/wasmtime/src/runtime/transaction/object_table.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Use shared latest objects during persistent GC"
```

---

## Task 12: Multi-Store Linear Memory Behavior Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add shared linear memory storage test**

Add:

```rust
#[test]
fn two_stores_share_transactional_linear_memory_backend_bytes() {
    // Create one shared region runtime and file-backed or DAX-backed config.
    // Store A writes a committed linear memory range.
    // Store B opens/adopts the same backend through the shared runtime.
    // Store B reads the same address range.
    // Assert bytes match without copying through a recovery buffer.
}
```

Use file-backed temp storage for CI-safe coverage. Keep DAX-specific stress tests on the Optane machine.

- [ ] **Step 2: Add touched-range flush guard test**

Add:

```rust
#[test]
fn linear_memory_commit_flushes_only_touched_ranges_for_shared_runtime() {
    // Stage writes in one granule.
    // Commit.
    // Inspect test counters or metadata for pending_linear_undo_chunks.
    // Assert unrelated granules are not marked for flush/undo retirement.
}
```

If no counter exists, add a crate-private test hook on `TxDurableLog` or `TMemory` that reports committed touched granule indices. Do not add a production data structure just for this assertion.

- [ ] **Step 3: Run focused tests**

Run:

```bash
cargo test -p wasmtime two_stores_share_transactional_linear_memory_backend_bytes --lib
cargo test -p wasmtime linear_memory_commit_flushes_only_touched_ranges_for_shared_runtime --lib
```

Expected: shared backend bytes are visible across stores and commit metadata is restricted to touched ranges.

- [ ] **Step 4: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/tests.rs \
        crates/wasmtime/src/runtime/transaction/state.rs \
        crates/wasmtime/src/runtime/vm/memory/tmemory.rs
git commit -m "Test multi-store transactional linear memory behavior"
```

---

## Task 13: End-To-End Runtime Tests

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add JVM-style behavior test**

Add one integration-style unit test:

```rust
#[test]
fn multi_store_transactions_behave_like_threads_on_shared_persistent_heap() {
    // Shared runtime.
    // Store A and Store B attached.
    // Shared persistent object root.
    // Store A updates object field in a transaction and commits.
    // Store B starts after A commits and observes the new field value.
    // Store B updates a different object and commits.
    // Persistent GC mark sees both committed updates from the shared runtime.
}
```

- [ ] **Step 2: Add concurrent conflict test with real threads**

Add:

```rust
#[test]
fn concurrent_store_threads_coordinate_object_conflicts_through_shared_runtime() {
    // Spawn two Rust threads.
    // Each thread creates its own Store and attaches to the same TransactionRegionRuntime.
    // Both attempt to write the same persistent object.
    // Assert one commits and the other receives the configured conflict result.
}
```

Use `std::thread::scope` so borrowed test fixtures do not need `'static` lifetimes. Use a `Barrier` so both threads contend at the same point.

- [ ] **Step 3: Run focused tests**

Run:

```bash
cargo test -p wasmtime multi_store_transactions_behave_like_threads_on_shared_persistent_heap --lib
cargo test -p wasmtime concurrent_store_threads_coordinate_object_conflicts_through_shared_runtime --lib
```

Expected: stores behave as execution contexts over the shared persistent heap, and conflicts are coordinated by the shared runtime.

- [ ] **Step 4: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add end-to-end multi-store transaction tests"
```

---

## Task 14: Full Verification

**Files:**
- No source edits unless verification exposes a bug.

- [ ] **Step 1: Format touched Rust code**

Run:

```bash
cargo fmt --all
```

Expected: formatting completes. If repository-wide pre-existing formatting drift appears, format only touched files and record the drift in the final report.

- [ ] **Step 2: Run transaction tests**

Run:

```bash
cargo test -p wasmtime transaction --lib
```

Expected: all transaction unit tests pass.

- [ ] **Step 3: Run package tests**

Run:

```bash
cargo test -p wasmtime
```

Expected: package tests pass.

- [ ] **Step 4: Check whitespace**

Run:

```bash
git diff --check
```

Expected: no whitespace errors.

- [ ] **Step 5: Commit verification fixes**

If verification required code fixes:

```bash
git add crates/wasmtime/src/runtime/transaction \
        crates/wasmtime/src/runtime/store.rs \
        crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Stabilize multi-store transactional runtime"
```

If no fixes were needed, do not create an empty commit.

---

## Behavioral Acceptance Criteria

- Two stores attached to the same `TransactionRegionRuntime` can access the same persistent object identity.
- A committed object update from one store becomes visible to another store without reopening storage.
- Store-local object table entries are caches; stale entries refresh from shared runtime metadata before dereference.
- Object conflict detection uses shared runtime authority, not per-store object table versions.
- Persistent object recovery seeds shared runtime metadata so newly attached stores can materialize mapped records.
- Persistent GC traces latest shared object records while remaining single-coordinator.
- Shared linear memory backend bytes are visible across stores.
- Existing single-store transactional object tests still pass.

## Self-Review

- Spec coverage: The plan covers shared runtime authority, store-local cache/workspace, object dereference behavior, linear memory behavior, commit publication, recovery, GC, and concurrency tests.
- Placeholder scan: The plan avoids unresolved placeholder markers and names concrete files, methods, tests, and commands.
- Type consistency: New shared directory types are introduced in Task 1 and reused consistently by runtime, object table, state, recovery, and tests.
- Scope check: MVCC, global store synchronization, and durable log typing changes are excluded so this can land as an incremental runtime authority refactor.
