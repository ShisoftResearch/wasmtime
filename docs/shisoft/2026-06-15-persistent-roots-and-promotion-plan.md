# Persistent Roots And Promotion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Make persistent roots a first-class runtime and durable-log concept, then use that foundation for commit-time promotion from volatile Wasmtime refs to persistent `ObjectId` graphs.

**Architecture:** The first wave keeps the Zen-style volatile object index. `TGlobal` and `TTable` reference commits publish durable root records in the same transaction log stream as object publications, and the store keeps a volatile committed-root index for GC root enumeration. Later waves promote ordinary volatile `VMGcRef` graphs into persistent `ObjectId` records before those refs can enter persistent roots or payloads.

**Tech Stack:** Rust, Wasmtime runtime internals, transactional durable log, file-backed recovery tests, `ObjectTable`, `TransactionState`, `PersistentGcState`, `cargo test`.

**Status:** Completed on 2026-06-15. The implemented form uses exact
per-table-element persistent root keys, instance-aware global/table root
packing, append-after-reopen stream reuse, recovered root-version seeding, and
an explicit promotion-required diagnostic for unknown or non-persistent refs.

---

## Scope

This plan intentionally does not implement durable block/chunk retirement,
persistent block reuse, `ObjectId` reuse, or a persistent object table. It also
does not move ordinary Wasmtime GC storage into persistent memory. The goal is
to make roots and promotion semantics trustworthy enough for those later waves.

## File Map

- `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Add non-test root-publication constructors and tests for root payload
    encoding.
- `crates/wasmtime/src/runtime/transaction.rs`
  - Add a volatile committed persistent-root index to `TransactionState`.
  - Add helpers to compute staged root publications and staged root deltas.
  - Seed commit-coupled persistent GC from the committed-root index after
    invalidating commits.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Publish root records before LP in `transaction_commit_impl`.
  - Apply the committed root index after the commit is complete.
- `crates/wasmtime/src/runtime/store.rs`
  - Seed recovered persistent roots and root versions when opening file-backed
    storage.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Reuse recovered stream cursors so file-backed transaction logs can append
    after reopen.
- `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Already decodes root records; add regression tests only if new real commit
    coverage needs a narrower recovery assertion.
- `docs/shisoft/transactional-wasm-runtime-core-design.md`
  - Record that roots are now durable records plus a volatile root index.
- `docs/shisoft/transactional-wasm-implementation-log.md`
  - Record verification after each completed wave.

## Task 1: Durable Root Publication Constructors

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `cargo test -p wasmtime --lib persistent_root_publication -- --format terse`

- [x] **Step 1: Write failing tests**

Add tests in `persist.rs` named:

```text
persistent_root_publication_encodes_global_object_id
persistent_root_publication_encodes_table_object_ids
```

The global test should construct:

```rust
let publication = PendingPublication::persistent_global_root(
    0,
    7,
    [Some(41_u64)],
).unwrap();
assert_eq!(publication.logical_id, (PackedGranuleDomain::TGlobal as u64) << 60);
assert_eq!(publication.version, 7);
assert_eq!(publication.kind, PackedGranuleDomain::TGlobal as u16);
assert_eq!(publication.type_layout_id, 0);
assert_eq!(publication.payload, (42_u64).to_le_bytes());
```

The table test should construct:

```rust
let publication = PendingPublication::persistent_table_root(
    3,
    9,
    [Some(41_u64), None, Some(99_u64)],
).unwrap();
let logical_id = ((PackedGranuleDomain::TTable as u64) << 60) | 3;
assert_eq!(publication.logical_id, logical_id);
assert_eq!(publication.version, 9);
assert_eq!(publication.kind, PackedGranuleDomain::TTable as u16);
assert_eq!(publication.type_layout_id, 0);
let mut expected = Vec::new();
expected.extend_from_slice(&42_u64.to_le_bytes());
expected.extend_from_slice(&0_u64.to_le_bytes());
expected.extend_from_slice(&100_u64.to_le_bytes());
assert_eq!(publication.payload, expected);
```

- [x] **Step 2: Verify the tests fail**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_publication -- --format terse
```

Expected: compile failure because the constructors do not exist.

- [x] **Step 3: Implement constructors**

Add this helper and constructors to `impl PendingPublication`:

```rust
fn persistent_root(
    domain: PackedGranuleDomain,
    root_index: u64,
    version: u32,
    roots: impl IntoIterator<Item = Option<u64>>,
) -> Result<Self> {
    ensure!(
        matches!(domain, PackedGranuleDomain::TGlobal | PackedGranuleDomain::TTable),
        "persistent root publication domain must be TGlobal or TTable"
    );
    let logical_id = ((domain as u64) << 60) | root_index;
    let mut payload = Vec::new();
    for root in roots {
        let encoded = match root {
            Some(object_id) => object_id
                .checked_add(1)
                .context("persistent root object id encoding overflow")?,
            None => 0,
        };
        payload.extend_from_slice(&encoded.to_le_bytes());
    }
    Ok(Self {
        logical_id,
        version,
        kind: domain as u16,
        type_layout_id: 0,
        payload,
    })
}

pub(crate) fn persistent_global_root(
    global_index: u64,
    version: u32,
    roots: impl IntoIterator<Item = Option<u64>>,
) -> Result<Self> {
    Self::persistent_root(PackedGranuleDomain::TGlobal, global_index, version, roots)
}

pub(crate) fn persistent_table_root(
    table_index: u64,
    version: u32,
    roots: impl IntoIterator<Item = Option<u64>>,
) -> Result<Self> {
    Self::persistent_root(PackedGranuleDomain::TTable, table_index, version, roots)
}
```

- [x] **Step 4: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_publication -- --format terse
```

Expected: both tests pass.

- [x] **Step 5: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "Add persistent root publication constructors"
```

## Task 2: Volatile Committed Root Index

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_root_index -- --format terse`

- [x] **Step 1: Write failing tests**

Add tests named:

```text
persistent_root_index_tracks_committed_global_root
persistent_root_index_removes_global_root_on_non_ref_commit
persistent_root_index_tracks_table_element_roots
```

The global test should stage `GlobalSnapshot::GcRef(0x701)` for a persistent
object mapped to `ObjectId { object_index: 1 }`, call a new helper
`staged_persistent_root_delta_for_test`, apply it after `complete_commit`, and
assert:

```rust
assert_eq!(
    state.persistent_root_ids_for_test(),
    object_set([ObjectId { object_index: 1 }])
);
```

The removal test should first install that root, then commit
`GlobalSnapshot::I32(0)` for the same global and assert the root set is empty.

The table test should stage two table elements in the same table, one persistent
ref and one scalar/null ref, then assert only the persistent object id remains
in `persistent_root_ids_for_test()`.

- [x] **Step 2: Verify the tests fail**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_index -- --format terse
```

Expected: compile failure because the root-index helpers do not exist.

- [x] **Step 3: Add root index data structures**

Add a private committed-root key and a `persistent_roots` field to
`TransactionState`:

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PersistentRootKey {
    Global {
        instance: Option<u32>,
        global_index: u32,
    },
    TableElement(TableElementKey),
    Recovered,
}

persistent_roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
persistent_root_versions: BTreeMap<PersistentRootKey, u32>,
```

Initialize it in `TransactionState::new`, and do not include it in
`TransactionWorkspace`; it is committed store state, not transaction-private
workspace.

Add:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PersistentRootDelta {
    roots: BTreeMap<PersistentRootKey, BTreeSet<ObjectId>>,
}

impl PersistentRootDelta {
    fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}
```

- [x] **Step 4: Compute staged root deltas**

Add `TransactionState::staged_persistent_root_delta`:

```rust
fn staged_persistent_root_delta(
    &self,
    object_table: &ObjectTable,
) -> Result<PersistentRootDelta> {
    self.ensure_active()?;
    let mut delta = PersistentRootDelta::default();
    for (&granule, &value) in &self.staged_globals {
        let GranuleId::TGlobal {
            instance,
            global_index,
        } = granule
        else {
            bail!("staged global map contains non-global key");
        };
        let mut roots = BTreeSet::new();
        if let Some(root) = persistent_root_object_id_for_global_snapshot(object_table, value)? {
            roots.insert(root);
        }
        delta
            .roots
            .insert(PersistentRootKey::Global { instance, global_index }, roots);
    }
    for (key, &value) in &self.staged_table_elements {
        let root_key = PersistentRootKey::TableElement(key);
        let roots = delta.roots.entry(root_key).or_default();
        if let Some(root) = persistent_root_object_id_for_table_element_snapshot(object_table, value)? {
            roots.insert(root);
        }
    }
    Ok(delta)
}
```

The final implementation tracks each table element independently so partial
table updates preserve untouched table-element roots.

- [x] **Step 5: Apply committed root deltas and enumerate roots**

Add:

```rust
fn apply_committed_persistent_root_delta(&mut self, delta: PersistentRootDelta) -> Result<()> {
    ensure!(
        self.active.is_none() && current_thread_transaction().is_none(),
        "committed persistent root delta can only be applied after complete_commit"
    );
    let next_versions = delta
        .roots
        .keys()
        .copied()
        .map(|key| {
            let next_version = self
                .persistent_root_versions
                .get(&key)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .context("persistent root version overflow")?;
            Ok((key, next_version))
        })
        .collect::<Result<Vec<_>>>()?;
    for (key, roots) in delta.roots {
        if roots.is_empty() {
            self.persistent_roots.remove(&key);
        } else {
            self.persistent_roots.insert(key, roots);
        }
    }
    for (key, version) in next_versions {
        self.persistent_root_versions.insert(key, version);
    }
    Ok(())
}

pub(crate) fn persistent_root_ids(&self) -> BTreeSet<ObjectId> {
    self.persistent_roots
        .values()
        .flat_map(|roots| roots.iter().copied())
        .collect()
}
```

Add `#[cfg(test)]` wrappers:

```rust
fn staged_persistent_root_delta_for_test(&self, objects: &ObjectTable) -> Result<PersistentRootDelta>;
fn apply_committed_persistent_root_delta_for_test(&mut self, delta: PersistentRootDelta);
fn persistent_root_ids_for_test(&self) -> BTreeSet<ObjectId>;
```

- [x] **Step 6: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_index -- --format terse
```

Expected: all three tests pass.

- [x] **Step 7: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Track committed persistent roots"
```

## Task 3: Publish Root Records In Real Commits

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `cargo test -p wasmtime --lib persistent_root_commit -- --format terse`
- Test: `cargo test -p wasmtime --test transaction_persistence -- --format terse`

- [x] **Step 1: Write failing tests**

Add a unit test named:

```text
persistent_root_commit_publishes_global_root_record
```

It should commit through `transaction_commit_impl` or the nearest existing
file-backed helper rather than manually calling
`publish_committed_global_object_root`. The assertion should recover the
file-backed region and verify:

```rust
assert_eq!(recovered_region.root_object_ids, vec![root.object_index]);
```

Add a root-only commit test where no object payload is staged in the same
transaction. This catches the final-marker case where the root publication is
the only durable object-data publication.

- [x] **Step 2: Verify the tests fail**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_commit -- --format terse
```

Expected: fail because the real commit path does not write root publications.

- [x] **Step 3: Build root publications from staged root delta**

Add `TransactionState::persistent_root_publications`:

```rust
fn persistent_root_publications(
    &self,
    delta: &PersistentRootDelta,
) -> Result<Vec<persist::PendingPublication>> {
    let mut publications = Vec::new();
    for (&key, roots) in &delta.roots {
        let version = self
            .persistent_root_versions
            .get(&key)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .context("persistent root publication version overflow")?;
        match key {
            PersistentRootKey::Global { global_index, .. } => {
                publications.push(persist::PendingPublication::persistent_global_root(
                    pack_persistent_global_root_index(instance, global_index)?,
                    version,
                    roots.iter().map(|root| Some(root.object_index)).chain(
                        roots.is_empty().then_some(None),
                    ),
                )?);
            }
            PersistentRootKey::TableElement(key) => {
                publications.push(persist::PendingPublication::persistent_table_root(
                    pack_persistent_table_root_index(key)?,
                    version,
                    roots.iter().map(|root| Some(root.object_index)),
                )?);
            }
            PersistentRootKey::Recovered => {}
        }
    }
    Ok(publications)
}
```

The empty-global case must encode one `None` slot so recovery knows the latest
global root version contains no root. If the recovery code cannot distinguish
that from no winner yet, update recovery tests before implementation.

- [x] **Step 4: Wire `transaction_commit_impl`**

In `transaction_commit_impl`:

1. Commit object payloads and collect object publications.
2. Compute `root_delta` while the transaction is active.
3. Convert `root_delta` into root publications.
4. Append root publications to object publications.
5. Publish all publications before LP.
6. After `complete_commit()`, apply `root_delta` to `TransactionState`.
7. Observe persistent GC with roots from the committed root index.

The root delta must be computed before `complete_commit()` because
`clear_active()` clears staged globals and table elements.

- [x] **Step 5: Seed GC from the committed root index after invalidation**

When `PersistentGcCommitDelta::invalidates_reachable_cache` is true, create the
new `PersistentGcState` with `self.persistent_root_ids()` rather than an empty
root list. Keep the existing best-effort behavior after commit.

- [x] **Step 6: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_commit -- --format terse
cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: root commit tests pass, commit-coupled GC tests still pass, and
file-backed persistence tests still pass.

- [x] **Step 7: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Publish persistent roots during commit"
```

## Task 4: Recovery Root Reinstallation Hook

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Test: `cargo test -p wasmtime --lib persistent_root_recovery_installs_root_index_for_gc -- --format terse`

- [x] **Step 1: Write failing test**

Add:

```text
persistent_root_recovery_installs_root_index_for_gc
```

Build a recovered region with root ids `[41]`, rebuild reachable objects, call a
new `install_recovered_persistent_roots_for_test(&[41])`, then assert:

```rust
assert_eq!(
    state.persistent_root_ids_for_test(),
    object_set([ObjectId { object_index: 41 }])
);
```

- [x] **Step 2: Implement recovery hook**

Add:

```rust
pub(crate) fn install_recovered_persistent_roots<I>(&mut self, roots: I)
where
    I: IntoIterator<Item = ObjectId>,
{
    let roots = roots.into_iter().collect::<BTreeSet<_>>();
    if roots.is_empty() {
        self.persistent_roots.remove(&PersistentRootKey::Recovered);
    } else {
        self.persistent_roots.insert(PersistentRootKey::Recovered, roots);
    }
    self.persistent_gc_state = None;
}
```

Use `PersistentRootKey::Recovered` for this catch-all root set. Recovered root
ids are already deduplicated by recovery; they are not a lockable transaction
granule.

The landed implementation also installs recovered root versions from recovered
durable winners:

```rust
pub(crate) fn install_recovered_persistent_root_state<I, J>(
    &mut self,
    roots: I,
    versions: J,
) -> Result<()>
where
    I: IntoIterator<Item = ObjectId>,
    J: IntoIterator<Item = (u64, u32)>,
```

This prevents reopened file-backed stores from republishing version `1` for an
existing persistent root stream.

- [x] **Step 3: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_root_recovery_installs_root_index_for_gc -- --format terse
```

Expected: test passes.

- [x] **Step 4: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs
git commit -m "Install recovered persistent roots"
```

## Task 5: Promotion Boundary Scaffolding

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Test: `cargo test -p wasmtime --lib persistent_ref_promotion_boundary -- --format terse`

- [x] **Step 1: Write failing tests**

Add tests named:

```text
persistent_root_commit_rejects_unknown_volatile_gc_ref
persistent_object_payload_commit_rejects_unknown_volatile_gc_ref
```

Both tests should prove the current branch never silently persists a raw
`VMGcRef`. A staged root or payload ref may commit only if
`ObjectTable::known_persistent_object_id_for_gc_ref` maps it to a persistent
`ObjectId`; otherwise the transaction aborts with a clear promotion-needed
diagnostic.

- [x] **Step 2: Implement explicit diagnostics**

Replace any generic missing-map errors on persistent root/payload commit with:

```text
volatile GC reference promotion into persistent object graph is not implemented yet
```

Do not add partial promotion yet. This task hardens the boundary and prevents
silent durability bugs.

- [x] **Step 3: Verify**

Run:

```bash
cargo test -p wasmtime --lib persistent_ref_promotion_boundary -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
```

Expected: boundary tests pass and transaction tests remain green.

- [x] **Step 4: Commit**

Run:

```bash
git add crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/vm/libcalls.rs
git commit -m "Harden persistent ref promotion boundary"
```

## Completion Criteria

- Real transaction commits publish durable root records for persistent
  `TGlobal`/`TTable` references.
- `TransactionState` can enumerate current committed persistent root
  `ObjectId`s without scanning durable logs.
- Commit-coupled GC after invalidation seeds from the committed root index.
- File-backed recovery sees root records produced by the real commit path, not
  only by test-only region helpers.
- Reopened file-backed storage seeds persistent root versions before appending
  more publications.
- Unknown volatile `VMGcRef` values cannot silently enter persistent roots or
  payloads.

## Deferred After This Plan

- Recursive promotion of ordinary volatile Wasmtime GC graphs into persistent
  `ObjectId` graphs.
- GC-facing integration with ordinary Wasmtime root closure.
- Durable block/chunk retirement and persistent block reuse.
