# Transactional Wasm Object Log Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route persistent object updates through ordinary transaction log entries and rebuild the volatile object index for persistent objects from committed object-log winners plus durable roots.

**Architecture:** Persistent object updates should not invent a separate recovery log or a persisted object table. A committed `tstruct`/`tarray` update appends one object data record and one ordinary transaction log entry whose `logical_id` is an object granule (`TStruct`/`TArray` carrying `ObjectId`). Recovery replays those log entries, selects the highest committed `version` per `ObjectId`, rebuilds the volatile object index from the winning object records, then restores durable roots from committed `TGlobal`/`TTable` updates.

**Tech Stack:** Rust, Wasmtime runtime transaction subsystem, object heap/object table code in `crates/wasmtime/src/runtime/transaction`, durable log/data codecs in `crates/wasmtime/src/runtime/vm/memory/tmemory`, file-backed restart tests in `crates/wasmtime/tests`.

---

## File Structure

- `crates/wasmtime/src/runtime/transaction.rs`
  - Owns `ObjectTable`, transaction commit wiring, and the recovery rebuild entry point for the volatile object index.
- `crates/wasmtime/src/runtime/transaction/object_heap.rs`
  - Owns durable object-record header/bytes helpers and object-record loading helpers used after recovery chooses a winning log entry.
- `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Owns `PendingPublication` and shared publication helpers. It should become the single place where object updates join ordinary granule updates in the durable publication path.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
  - Owns `PackedGranuleDomain`, `TxLogEntry`, `TxDataRecordHeader`, and any helper functions needed to pack/unpack object granule logical ids.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Owns log replay, object winner extraction, root replay, and the recovery product consumed by `ObjectTable`.
- `crates/wasmtime/src/lib.rs`
  - Owns narrow `_internal` re-exports if integration tests need object-recovery smoke hooks.
- `crates/wasmtime/tests/transaction_persistence.rs`
  - Owns file-backed smoke tests that prove object updates and object roots survive reopen/recovery.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Records the new object-log recovery milestone once tests pass.

## Task 1: Add Persistent Object Granule Identity Helpers

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write the failing identity tests**

```rust
#[test]
fn packed_granule_id_roundtrips_struct_object_id() {
    let logical_id = pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap();
    let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TStruct);
    assert_eq!(object_id, 41);
}

#[test]
fn packed_granule_id_roundtrips_array_object_id() {
    let logical_id = pack_object_granule_id(PackedGranuleDomain::TArray, 99).unwrap();
    let (domain, object_id) = unpack_object_granule_id(logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TArray);
    assert_eq!(object_id, 99);
}

#[test]
fn object_publication_builds_object_logical_id() {
    let pub_ = PendingPublication::persistent_object_for_test(
        PackedGranuleDomain::TStruct,
        41,
        7,
        12,
        &[1, 2, 3],
    );
    let (domain, object_id) = unpack_object_granule_id(pub_.logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TStruct);
    assert_eq!(object_id, 41);
    assert_eq!(pub_.version, 7);
    assert_eq!(pub_.type_info, 12);
}
```

- [ ] **Step 2: Run the identity tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib packed_granule_id_roundtrips_struct_object_id -- --exact
cargo test -p wasmtime --lib packed_granule_id_roundtrips_array_object_id -- --exact
cargo test -p wasmtime --lib object_publication_builds_object_logical_id -- --exact
```

Expected: FAIL because object-granule pack/unpack helpers and object publication constructors do not exist yet.

- [ ] **Step 3: Add the helpers**

```rust
#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PackedGranuleDomain {
    TMemory = 1,
    TMemorySize = 2,
    TGlobal = 3,
    TTable = 4,
    TTableSize = 5,
    TStruct = 6,
    TArray = 7,
}

pub(crate) fn pack_object_granule_id(
    domain: PackedGranuleDomain,
    object_id: u64,
) -> Result<u64> {
    match domain {
        PackedGranuleDomain::TStruct | PackedGranuleDomain::TArray => {
            ensure!(object_id < (1u64 << 60), "ObjectId does not fit PackedGranuleId payload");
            Ok(((domain as u64) << 60) | object_id)
        }
        _ => bail!("domain {:?} is not an object granule", domain),
    }
}

pub(crate) fn unpack_object_granule_id(logical_id: u64) -> Result<(PackedGranuleDomain, u64)> {
    let domain_bits = u16::try_from(logical_id >> 60).unwrap();
    let object_id = logical_id & ((1u64 << 60) - 1);
    let domain = match domain_bits {
        6 => PackedGranuleDomain::TStruct,
        7 => PackedGranuleDomain::TArray,
        _ => bail!("logical id {logical_id:#x} is not an object granule"),
    };
    Ok((domain, object_id))
}

impl PendingPublication {
    #[cfg(test)]
    fn persistent_object_for_test(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_info: u32,
        payload: &[u8],
    ) -> Self {
        Self {
            logical_id: pack_object_granule_id(domain, object_id).unwrap(),
            version,
            kind: domain as u16,
            type_info,
            payload: payload.to_vec(),
        }
    }
}
```

- [ ] **Step 4: Run the identity tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib packed_granule_id_roundtrips_struct_object_id -- --exact
cargo test -p wasmtime --lib packed_granule_id_roundtrips_array_object_id -- --exact
cargo test -p wasmtime --lib object_publication_builds_object_logical_id -- --exact
git diff --check
```

Expected: PASS for all three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/durable_log.rs \
  crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "Add persistent object granule identity helpers"
```

## Task 2: Route Object Updates Through The Shared Transaction Publication Path

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write the failing object-publication tests**

```rust
#[test]
fn commit_publishes_struct_object_update() {
    let mut recorder = RecordingDurability::default();
    let mut objects = ObjectTable::default();
    let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();

    let pub_ = objects.pending_publication_for_test(object).unwrap();
    let (domain, object_id) = unpack_object_granule_id(pub_.logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TStruct);
    assert_eq!(object_id, object.object_index);
    assert_eq!(pub_.version, 1);
}

#[test]
fn commit_publishes_array_object_update() {
    let mut objects = ObjectTable::default();
    let object = objects
        .allocate_array(vec![ObjectValue::Ref(None), ObjectValue::I64(9)])
        .unwrap();

    let pub_ = objects.pending_publication_for_test(object).unwrap();
    let (domain, object_id) = unpack_object_granule_id(pub_.logical_id).unwrap();
    assert_eq!(domain, PackedGranuleDomain::TArray);
    assert_eq!(object_id, object.object_index);
    assert_eq!(pub_.version, 1);
}
```

- [ ] **Step 2: Run the object-publication tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib commit_publishes_struct_object_update -- --exact
cargo test -p wasmtime --lib commit_publishes_array_object_update -- --exact
```

Expected: FAIL because object updates do not yet produce shared `PendingPublication`s.

- [ ] **Step 3: Add object publication wiring**

```rust
impl ObjectTable {
    fn object_pending_publication(&self, object_id: ObjectId) -> Result<PendingPublication> {
        let handle = self.live_slot(object_id)?.current_record;
        let header = self.heap.header(handle)?;
        let bytes = self.heap.record_bytes(handle)?;
        let domain = match object_kind_from_u16(header.kind)? {
            ObjectKind::Struct => PackedGranuleDomain::TStruct,
            ObjectKind::Array => PackedGranuleDomain::TArray,
            other => bail!("object kind {other:?} is not a persistent object granule"),
        };
        Ok(PendingPublication {
            logical_id: pack_object_granule_id(domain, header.object_id)?,
            version: header.version,
            kind: domain as u16,
            type_info: header.type_index,
            payload: bytes,
        })
    }

    #[cfg(test)]
    fn pending_publication_for_test(&self, object_id: ObjectId) -> Result<PendingPublication> {
        self.object_pending_publication(object_id)
    }
}

impl TransactionState {
    fn collect_pending_publications(&self) -> Result<Vec<PendingPublication>> {
        let mut pubs = self.pending_linear_publications()?;
        pubs.extend(self.pending_object_publications()?);
        Ok(pubs)
    }
}
```

Do not create a separate object log buffer. Object updates must flow through the same `PendingPublication` list used by `tmemory`/`tglobal`/`ttable`.

- [ ] **Step 4: Run the object-publication tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib commit_publishes_struct_object_update -- --exact
cargo test -p wasmtime --lib commit_publishes_array_object_update -- --exact
git diff --check
```

Expected: PASS for both tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction/object_heap.rs \
  crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "Route persistent object updates through transaction publications"
```

## Task 3: Rebuild The Volatile Object Index From Committed Object Log Winners

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_heap.rs`

- [ ] **Step 1: Write the failing object-recovery tests**

```rust
#[test]
fn recovery_replays_latest_object_record_per_object_id() {
    let region = sample_region_with_competing_object_updates();
    let recovered = recover_region_for_test(&region).unwrap();
    let objects = recovered.object_winners.unwrap();
    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].object_id, 41);
    assert_eq!(objects[0].version, 9);
}

#[test]
fn recovery_rejects_duplicate_committed_object_version() {
    let region = sample_region_with_duplicate_object_version();
    let err = recover_region_for_test(&region).unwrap_err();
    assert!(err.to_string().contains("duplicate committed version"));
}

#[test]
fn object_index_rebuilds_from_recovered_object_winners() {
    let region = sample_region_with_two_object_winners();
    let recovered = recover_region_for_test(&region).unwrap();
    let mut objects = ObjectTable::default();
    objects.rebuild_from_recovery_for_test(&recovered.object_winners.unwrap()).unwrap();
    assert_eq!(objects.live_count(), 2);
    assert_eq!(objects.version(ObjectId { object_index: 41 }).unwrap(), 1);
}
```

- [ ] **Step 2: Run the object-recovery tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib recovery_replays_latest_object_record_per_object_id -- --exact
cargo test -p wasmtime --lib recovery_rejects_duplicate_committed_object_version -- --exact
cargo test -p wasmtime --lib object_index_rebuilds_from_recovered_object_winners -- --exact
```

Expected: FAIL because recovery does not yet surface object winners and `ObjectTable` cannot rebuild from replayed log winners.

- [ ] **Step 3: Add object-winner replay and object-table rebuild**

```rust
#[derive(Debug, Clone)]
pub(crate) struct RecoveredObjectWinner {
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_index: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_bytes: Vec<u8>,
}

impl RecoveredRegion {
    pub(crate) fn object_winners(&self) -> Result<Vec<RecoveredObjectWinner>> {
        self.winners
            .iter()
            .filter_map(|winner| {
                let (domain, object_id) = unpack_object_granule_id(winner.logical_id).ok()?;
                Some((domain, object_id, winner))
            })
            .map(|(domain, object_id, winner)| {
                let record = self.load_data_record(winner.data_block, winner.data_offset)?;
                let header = TxObjectHeader::read_from_prefix(&record)?;
                Ok(RecoveredObjectWinner {
                    object_id,
                    version: header.version,
                    kind: domain as u16,
                    type_index: header.type_index,
                    data_block: winner.data_block,
                    data_offset: winner.data_offset,
                    record_bytes: record,
                })
            })
            .collect()
    }
}

impl ObjectTable {
    fn rebuild_from_recovered_object_winners(
        &mut self,
        winners: &[RecoveredObjectWinner],
    ) -> Result<()> {
        self.slots.clear();
        self.free_list.clear();
        self.gc_ref_to_object.clear();
        self.object_to_gc_ref.clear();
        self.func_ref_to_object.clear();
        self.object_to_func_ref.clear();
        self.live_count = 0;
        self.next_version = 0;
        self.next_record_version = 0;

        for winner in winners {
            self.next_record_version = self.next_record_version.max(winner.version);
            self.install_recovered_object_winner(winner)?;
        }
        self.rebuild_free_list_holes()?;
        Ok(())
    }
}
```

Do not scan a separate persistent object table. The rebuild input must be the committed object-log winners.

- [ ] **Step 4: Run the object-recovery tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib recovery_replays_latest_object_record_per_object_id -- --exact
cargo test -p wasmtime --lib recovery_rejects_duplicate_committed_object_version -- --exact
cargo test -p wasmtime --lib object_index_rebuilds_from_recovered_object_winners -- --exact
git diff --check
```

Expected: PASS for all three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs \
  crates/wasmtime/src/runtime/transaction.rs \
  crates/wasmtime/src/runtime/transaction/object_heap.rs
git commit -m "Rebuild object index from committed object log winners"
```

## Task 4: Recover Durable Object Roots From `TGlobal` And `TTable` Winners

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`

- [ ] **Step 1: Write the failing root-recovery tests**

```rust
#[test]
fn recovery_rebuilds_persistent_object_root_from_tglobal() {
    let region = sample_region_with_object_rooted_by_global();
    let recovered = recover_region_for_test(&region).unwrap();
    assert_eq!(recovered.root_object_ids, vec![41]);
}

#[test]
fn recovery_rebuilds_persistent_object_roots_from_ttable() {
    let region = sample_region_with_object_rooted_by_table();
    let recovered = recover_region_for_test(&region).unwrap();
    assert_eq!(recovered.root_object_ids, vec![41, 42]);
}
```

- [ ] **Step 2: Run the root-recovery tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --lib recovery_rebuilds_persistent_object_root_from_tglobal -- --exact
cargo test -p wasmtime --lib recovery_rebuilds_persistent_object_roots_from_ttable -- --exact
```

Expected: FAIL because recovery does not yet decode durable object roots from committed `TGlobal`/`TTable` winners.

- [ ] **Step 3: Add root replay**

```rust
#[derive(Debug, Default)]
pub(crate) struct RecoveredRootSet {
    pub(crate) object_ids: Vec<u64>,
}

fn replay_durable_roots(region: &RecoveredRegion) -> Result<RecoveredRootSet> {
    let mut roots = RecoveredRootSet::default();
    for winner in &region.winners {
        match packed_granule_domain(winner.logical_id)? {
            PackedGranuleDomain::TGlobal => {
                if let Some(object_id) = decode_root_object_id(&region.load_data_record(
                    winner.data_block,
                    winner.data_offset,
                )?)? {
                    roots.object_ids.push(object_id);
                }
            }
            PackedGranuleDomain::TTable => {
                roots.object_ids.extend(decode_root_object_ids(
                    &region.load_data_record(winner.data_block, winner.data_offset)?,
                )?);
            }
            _ => {}
        }
    }
    roots.object_ids.sort_unstable();
    roots.object_ids.dedup();
    Ok(roots)
}
```

Keep roots represented through `TGlobal`/`TTable` winners. Do not add a separate root-only log kind.

- [ ] **Step 4: Run the root-recovery tests to verify they pass**

Run:

```bash
cargo test -p wasmtime --lib recovery_rebuilds_persistent_object_root_from_tglobal -- --exact
cargo test -p wasmtime --lib recovery_rebuilds_persistent_object_roots_from_ttable -- --exact
git diff --check
```

Expected: PASS for both tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs \
  crates/wasmtime/src/runtime/transaction.rs
git commit -m "Recover persistent object roots from transaction log winners"
```

## Task 5: Add File-Backed Persistent Object Recovery Smoke Tests

**Files:**
- Modify: `crates/wasmtime/src/lib.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Write the failing file-backed object recovery tests**

```rust
#[test]
fn file_backed_region_recovers_committed_struct_object() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[9, 8, 7]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 41);
    assert_eq!(recovered.object_winners[0].version, 1);
}

#[test]
fn file_backed_region_recovers_object_rooted_by_global() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rooted-object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[1, 2, 3]).unwrap();
    publish_committed_global_object_root(&path, 1, 41).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.root_object_ids, vec![41]);
}
```

- [ ] **Step 2: Run the file-backed object recovery tests to verify they fail**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_committed_struct_object -- --exact
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_object_rooted_by_global -- --exact
```

Expected: FAIL because the file-backed internal hooks do not yet publish or report persistent object winners/roots.

- [ ] **Step 3: Add narrow internal smoke-test hooks**

```rust
#[cfg(feature = "transaction")]
pub mod transaction_persistence {
    pub use crate::vm::block_region::{
        create_file_backed_region_image,
        publish_committed_struct_object,
        publish_committed_global_object_root,
        reopen_and_recover_file_backed_region,
        TransactionPersistenceRecoveredObjectWinner,
        TransactionPersistenceRecoveredRegion,
    };
}
```

```rust
pub struct TransactionPersistenceRecoveredObjectWinner {
    pub object_id: u64,
    pub version: u32,
}

pub struct TransactionPersistenceRecoveredRegion {
    pub winners: Vec<TransactionPersistenceRecoveredWinner>,
    pub object_winners: Vec<TransactionPersistenceRecoveredObjectWinner>,
    pub root_object_ids: Vec<u64>,
}
```

Update the implementation log with one new entry that says object-log recovery now rebuilds the volatile object index and roots from committed transaction-log winners.

- [ ] **Step 4: Run the file-backed object recovery tests and focused suite**

Run:

```bash
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_committed_struct_object -- --exact
cargo test -p wasmtime --test transaction_persistence file_backed_region_recovers_object_rooted_by_global -- --exact
cargo test -p wasmtime --lib recovery_replays_latest_object_record_per_object_id -- --exact
cargo test -p wasmtime --lib recovery_rebuilds_persistent_object_root_from_tglobal -- --exact
git diff --check
```

Expected: PASS for all four tests.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/lib.rs \
  crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs \
  crates/wasmtime/tests/transaction_persistence.rs \
  docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Add persistent object log recovery smoke tests"
```

## Plan Review

- This plan covers the unfinished object-table and metadata work discussed in
  the latest design session:
  - object updates must use ordinary transaction log entries
  - the volatile object index must be rebuilt from committed object-log winners
  - durable roots must come from committed `TGlobal`/`TTable` winners
  - file-backed restart smoke tests must prove the path works after reopen
- This plan intentionally excludes:
  - persistent GC / reclaim
  - deletion / tombstones
  - fast-restart `PersistentIndex`
  - full production bridge reconstruction for `VMGcRef` / `VMFuncRef`
- `ObjectId` remains the durable object identity and `version` remains `u32`
  across object record publication and recovery.
