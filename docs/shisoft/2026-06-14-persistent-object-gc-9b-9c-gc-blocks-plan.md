# Persistent Object GC 9B/9C GC Blocks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep Wave 9B commit-coupled marking, then implement Wave 9C durable GC tombstones in GC-owned metadata blocks instead of user transaction logs.

**Architecture:** Wave 9A marker code is refactored into reusable object-GC machinery. Wave 9B adds volatile commit-barrier marking. Wave 9C adds a fixed-size `GcTombstoneEntry` stream in dedicated GC metadata blocks; recovery applies those tombstones after selecting live object-data winners from transaction logs.

**Tech Stack:** Rust, Wasmtime runtime internals, `ObjectTable`, `ObjectHeap`, `VMemoryBlockRegion`, file-backed block regions, recovery tests, `cargo test`.

---

## Scope

This document supersedes the tombstone-through-transaction-log Tasks 5-8 in
`docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-implementation-plan.md`.
The older plan still contains useful Wave 9A refactor and Wave 9B marking
steps, but its transaction-log tombstone design is rejected.

Wave 9C in this plan does not reuse `ObjectId`s, reclaim object-data blocks,
compact live records, update Immix line marks, or persist a full object table.
It only records durable GC tombstones in GC-owned blocks and teaches recovery to
filter the rebuilt volatile object table.

## File Map

- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Owns persistent GC report types, mark state, commit deltas, and sweep plans.
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs`
  - Owns fixed-size GC tombstone entry encoding, checksum validation, and
    tombstone winner selection.
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
  - Re-exports object-GC types and exposes narrow object-table helpers.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
  - Adds `gc_tombstone` as a private module and re-exports test-visible helpers.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Adds GC tombstone metadata blocks and append/scan helpers.
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Applies committed GC tombstones after object-data winner selection.
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

## Task 1: Preserve The Wave 9A Refactor

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_object_marker -- --format terse`

- [ ] **Step 1: Move marker-only code into `object_gc.rs`**

Move `PersistentRootErrorKind`, `PersistentRootError`,
`DanglingObjectRefKind`, `DanglingObjectRef`,
`PersistentObjectMarkReport`, and `PersistentObjectMarker` from
`transaction.rs` into `object_gc.rs`. Keep `PersistentObjectMarker::mark` as
the public compatibility entry used by existing tests.

- [ ] **Step 2: Add budgeted mark state**

Add these runtime-only types in `object_gc.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentGcBudget {
    objects: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcStepReport {
    pub(crate) scanned_objects: usize,
    pub(crate) enqueued_objects: usize,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PersistentGcState {
    reachable: BTreeSet<ObjectId>,
    grey: VecDeque<ObjectId>,
    dangling_refs: Vec<DanglingObjectRef>,
    invalid_roots: Vec<PersistentRootError>,
}
```

`PersistentObjectMarker::mark` should construct `PersistentGcState`, run mark
steps until the grey queue is empty, and return the same report shape as Wave
9A.

- [ ] **Step 3: Verify marker compatibility**

Run:

```bash
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
```

Expected: all existing `persistent_object_marker` tests pass.

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Refactor persistent object marker core"
```

## Task 2: Keep Wave 9B Commit-Coupled Marking

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_commit -- --format terse`

- [ ] **Step 1: Add commit delta types**

Add these types to `object_gc.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct PersistentObjectEdge {
    pub(crate) from: ObjectId,
    pub(crate) to: ObjectId,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentGcCommitDelta {
    pub(crate) new_roots: BTreeSet<ObjectId>,
    pub(crate) edges: BTreeSet<PersistentObjectEdge>,
}
```

Object publication commits should produce edges from the committed object to
each persistent `ObjectId` in its payload. Root commits from `tglobal` and
`ttable` produce `new_roots`.

- [ ] **Step 2: Add commit barrier observation**

Add:

```rust
impl PersistentGcState {
    pub(crate) fn observe_commit_delta(
        &mut self,
        objects: &ObjectTable,
        delta: &PersistentGcCommitDelta,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport> {
        for &root in &delta.new_roots {
            self.enqueue_reachable(root);
        }
        for edge in &delta.edges {
            if self.reachable.contains(&edge.from) {
                self.enqueue_reachable(edge.to);
            }
        }
        self.mark_step(objects, budget)
    }
}
```

If `A -> B` is published and `A` is unmarked, do not enqueue `B` immediately;
scanning `A` in the same cycle will see the committed edge.

- [ ] **Step 3: Add focused tests**

Add tests named:

```text
persistent_gc_commit_barrier_enqueues_new_root
persistent_gc_commit_barrier_enqueues_child_when_owner_is_marked
persistent_gc_commit_barrier_ignores_child_when_owner_is_unmarked
```

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
```

Expected: all three tests pass.

- [ ] **Step 4: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Add commit-coupled persistent marking"
```

## Task 3: Add Fixed-Size GC Tombstone Entries

**Files:**
- Create: `crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Test: `cargo test -p wasmtime --lib gc_tombstone_entry -- --format terse`

- [ ] **Step 1: Write entry codec tests**

Add tests in `gc_tombstone.rs`:

```rust
#[test]
fn gc_tombstone_entry_round_trips_and_validates_crc() {
    let mut entry = GcTombstoneEntry::new(41, 7, 3);
    entry.seal_crc32();
    let bytes = entry.as_bytes();
    let decoded = GcTombstoneEntry::from_bytes(bytes).unwrap();
    assert_eq!(decoded.object_id, 41);
    assert_eq!(decoded.target_version, 7);
    assert_eq!(decoded.gc_epoch, 3);
}

#[test]
fn gc_tombstone_entry_rejects_corrupt_crc() {
    let mut entry = GcTombstoneEntry::new(41, 7, 3);
    entry.seal_crc32();
    let mut bytes = entry.as_bytes();
    bytes[8] ^= 0x55;
    assert!(GcTombstoneEntry::from_bytes(bytes).is_err());
}
```

- [ ] **Step 2: Implement the entry**

Create `gc_tombstone.rs` with a 32-byte entry:

```rust
pub(crate) const GC_TOMBSTONE_ENTRY_MAGIC: u32 = 0x5447_4347;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GcTombstoneEntry {
    pub(crate) magic: u32,
    pub(crate) reserved0: u32,
    pub(crate) object_id: u64,
    pub(crate) target_version: u32,
    pub(crate) gc_epoch: u32,
    pub(crate) reserved1: u32,
    pub(crate) crc32: u32,
}
```

`new(object_id, target_version, gc_epoch)` sets both reserved fields to zero.
`seal_crc32` computes CRC over bytes `0..28`. `from_bytes` checks the magic,
reserved fields, and checksum.

- [ ] **Step 3: Wire the module**

Add to `tmemory.rs`:

```rust
mod gc_tombstone;

pub(crate) use gc_tombstone::GcTombstoneEntry;
```

- [ ] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib gc_tombstone_entry -- --format terse
```

Expected: both entry tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs
git commit -m "Add GC tombstone entry codec"
```

## Task 4: Add GC Tombstone Metadata Blocks

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --lib gc_tombstone_blocks -- --format terse`

- [ ] **Step 1: Write append/scan tests**

Add tests that allocate a small region, append two tombstones, scan them back,
and reject a manually corrupted entry:

```text
gc_tombstone_blocks_scan_appended_entries
gc_tombstone_blocks_ignore_trailing_empty_entries
gc_tombstone_blocks_reject_corrupt_entry
```

- [ ] **Step 2: Add a block header**

Add to `gc_tombstone.rs`:

```rust
pub(crate) const GC_TOMBSTONE_BLOCK_MAGIC: u32 = 0x5447_4342;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GcTombstoneBlockHeader {
    pub(crate) magic: u32,
    pub(crate) block_seq: u32,
    pub(crate) entry_count: u32,
    pub(crate) next_block: u32,
}
```

The body is a dense array of `GcTombstoneEntry`. Empty tail bytes remain zero.
This stream is not a `TxLogEntry` stream and has no transaction id or LP bit.

- [ ] **Step 3: Add region append/scan helpers**

Add methods to `VMemoryBlockRegion` in `block_region.rs`:

```rust
pub(crate) fn append_gc_tombstone_for_test(
    &mut self,
    entry: GcTombstoneEntry,
) -> Result<()>;

pub(crate) fn scan_gc_tombstones_for_test(&self) -> Result<Vec<GcTombstoneEntry>>;
```

The implementation may reserve a single GC tombstone block for the first
version. When it fills, allocate another block from the global block allocator
and link it with `next_block`.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib gc_tombstone_blocks -- --format terse
```

Expected: append/scan tests pass.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Add GC tombstone metadata blocks"
```

## Task 5: Apply GC Tombstones During Recovery

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs`
- Test: `cargo test -p wasmtime --lib gc_tombstone_recovery -- --format terse`

- [ ] **Step 1: Write recovery tests**

Add tests named:

```text
gc_tombstone_recovery_suppresses_covered_live_object
gc_tombstone_recovery_keeps_newer_live_object
gc_tombstone_recovery_treats_partial_collection_as_valid
```

The first test creates a live object winner at version `7` and a GC tombstone
for `(object_id, target_version = 7)`, then expects no recovered object-table
winner. The second creates a live object at version `8` and a tombstone for
version `7`, then expects the live object to remain.

- [ ] **Step 2: Add tombstone winner selection**

Add helper:

```rust
pub(crate) fn latest_gc_tombstones(
    entries: impl IntoIterator<Item = GcTombstoneEntry>,
) -> BTreeMap<u64, GcTombstoneEntry> {
    let mut latest = BTreeMap::new();
    for entry in entries {
        latest
            .entry(entry.object_id)
            .and_modify(|current: &mut GcTombstoneEntry| {
                if entry.target_version > current.target_version
                    || (entry.target_version == current.target_version
                        && entry.gc_epoch > current.gc_epoch)
                {
                    *current = entry;
                }
            })
            .or_insert(entry);
    }
    latest
}
```

- [ ] **Step 3: Filter object winners**

After `replay_object_winners` selects live object-data winners, scan GC
tombstone blocks and filter:

```rust
let tombstones = latest_gc_tombstones(region.scan_gc_tombstones()?);
object_winners.retain(|winner| {
    tombstones
        .get(&winner.object_id)
        .is_none_or(|dead| dead.target_version < winner.version)
});
```

Do not modify object-data records. Do not write transaction log records.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib gc_tombstone_recovery -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: GC tombstone recovery tests pass and existing persistence tests remain
green.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/vm/memory/tmemory/gc_tombstone.rs
git commit -m "Apply GC tombstones during recovery"
```

## Task 6: Build Sweep Plans That Emit GC Tombstones

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_sweep -- --format terse`

- [ ] **Step 1: Write sweep-plan tests**

Add tests named:

```text
persistent_gc_sweep_plan_lists_unreachable_persistent_objects
persistent_gc_sweep_plan_builds_gc_tombstone_entries
```

The second test should assert that unreachable object `O` at current version
`V` produces a `GcTombstoneEntry::new(O.object_index, V, gc_epoch)`.

- [ ] **Step 2: Add sweep-plan types**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectTombstoneCandidate {
    pub(crate) object_id: ObjectId,
    pub(crate) target_version: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentObjectSweepPlan {
    pub(crate) tombstones: Vec<PersistentObjectTombstoneCandidate>,
}
```

`PersistentObjectSweepPlan::from_mark_report` reads
`report.unreachable_persistent` and records each object's current live version.
It does not create transaction publications.

- [ ] **Step 3: Convert candidates to GC entries**

Add:

```rust
impl PersistentObjectSweepPlan {
    pub(crate) fn gc_tombstone_entries(
        &self,
        gc_epoch: u32,
    ) -> Vec<GcTombstoneEntry> {
        self.tombstones
            .iter()
            .map(|candidate| {
                let mut entry = GcTombstoneEntry::new(
                    candidate.object_id.object_index,
                    candidate.target_version,
                    gc_epoch,
                );
                entry.seal_crc32();
                entry
            })
            .collect()
    }
}
```

- [ ] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_sweep -- --format terse
```

Expected: sweep-plan tests pass and no code references
`PendingPublication::persistent_object_tombstone`.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_gc.rs
git commit -m "Plan persistent object GC tombstones"
```

## Task 7: End-To-End File-Backed GC Tombstone Recovery

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed -- --format terse`

- [ ] **Step 1: Write the end-to-end test**

Create a file-backed region, publish a root object and an unreachable object
through ordinary object transaction logs, run full marker from the root, append
GC tombstones from the sweep plan into GC tombstone blocks, reopen the file, and
assert that recovery rebuilds only the root object.

Name the test:

```text
persistent_gc_tombstone_file_backed_recovery_drops_unreachable_object
```

- [ ] **Step 2: Verify the failure mode**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed_recovery_drops_unreachable_object -- --format terse
```

Expected before Task 5 is implemented: recovery rebuilds the unreachable object
because GC tombstone blocks are not applied.

- [ ] **Step 3: Apply the sweep plan through GC blocks**

The test should call only GC block APIs for tombstone persistence:

```rust
for entry in sweep.gc_tombstone_entries(1) {
    region.append_gc_tombstone_for_test(entry).unwrap();
}
```

It must not call `publish_object_publications_before_commit` for tombstones.

- [ ] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_gc_tombstone_file_backed -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: file-backed GC tombstone recovery passes and existing transaction
persistence tests remain green.

- [ ] **Step 5: Commit**

Run:

```bash
git diff --check
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "Recover file-backed GC tombstones"
```

## Task 8: Documentation And Final Verification

**Files:**
- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify: `docs/shisoft/transactional-wasm-remaining-work-roadmap.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [ ] **Step 1: Record implemented status**

Add this status text to the docs:

```markdown
Wave 9C durable deletion uses GC-owned tombstone blocks. Tombstones are
fixed-size GC metadata entries with `ObjectId`, `target_version`, `gc_epoch`,
and CRC32. They are not `TxLogEntry` records and are not published through user
transaction streams. Recovery first selects live object-data winners from
transaction logs, then applies GC tombstones to decide which `ObjectId`s are
omitted from the rebuilt volatile object table.
```

- [ ] **Step 2: Run final verification**

Run:

```bash
cargo fmt --check
cargo test -p wasmtime --lib persistent_gc -- --format terse
cargo test -p wasmtime --lib gc_tombstone -- --format terse
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test -p wasmtime --lib -- --format terse
git diff --check
```

Expected results:

```text
persistent_gc filters: all pass
gc_tombstone filters: all pass
persistent_object_marker filters: all pass
transaction filter: all pass
transaction_persistence test: all pass
wasmtime lib tests: all pass with the existing ignored count only
git diff --check: no output
```

- [ ] **Step 3: Commit docs**

Run:

```bash
git add docs/shisoft/transactional-wasm-runtime-core-design.md docs/shisoft/transactional-wasm-remaining-work-roadmap.md docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document GC tombstone blocks"
```

## Completion Criteria

- Wave 9A `PersistentObjectMarker::mark` remains available and passes existing
  tests.
- Wave 9B commit barriers advance the volatile marker without scanning staged
  transaction workspaces.
- Wave 9C tombstones are fixed-size GC metadata entries in GC-owned blocks.
- No GC tombstone is encoded as `TxLogEntry`, `PendingPublication`, transaction
  LP, or object-data tombstone payload.
- Recovery applies GC tombstones after selecting live object-data winners.
- A crash during GC tombstone writing can leave a partial collection; valid
  entries still apply and missing entries only delay collection.
- `ObjectId` reuse remains disabled.

## Rollback Plan

- If GC tombstone block allocation is not ready, keep the fixed-size entry
  codec and recovery filter tests using an in-memory entry vector.
- If file-backed tombstone blocks expose block allocator issues, keep the
  in-memory block-region tests and split file-backed append/reopen support into
  a separate task.
- If commit-coupled marking conflicts with current commit paths, keep the
  marker and GC tombstone block work independent; 9C can run from a full
  stop-the-world marker until the 9B epilogue is ready.
