# Serializable MVCC with Optimistic Concurrency Control Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add feature-gated, volatile MVCC visibility to every transaction domain and pair it with commit-time optimistic certification so transactions are serializable and opaque, including prevention of write skew.

**Architecture:** Keep MVCC, concurrency control, persistent GC, and storage selection as independent policy axes. `transaction-mvcc` selects a typed sidecar visibility facade; the first supported concurrency adapter is the existing `transaction-cc-optimistic-validation` policy, extended with short commit-time shared/exclusive certification latches. Existing copy-on-write durability, commit-LP publication, recovery formats/decision rules, and backend selection remain the current-state publication layer beneath MVCC. The user-approved exception is a narrow set of capacity-only tmemory primitives used by MVCC commit-time growth-tail publication and cross-build undo application; creation/publication entry points are MVCC-feature-gated, while the capacity restore primitive compiles without MVCC. Ordinary logical APIs and non-MVCC publication behavior remain unchanged.

**Tech Stack:** Rust, Cargo feature gates, `Arc`, `Mutex`, `Condvar`, atomic commit records, existing `TransactionState`/`TransactionWorkspace`, existing `TransactionRegionRuntime`, existing `GranuleId` domains, VMemory/file-backed/DAX PMEM tmemory backends, transaction unit and persistence integration tests.

## Global Constraints

- Follow the approved design in `docs/superpowers/specs/2026-07-30-serializable-mvcc-design.md`.
- Follow the Bytecode Alliance AI Tool Use Policy.
- Do not open, review, or comment on a Wasmtime pull request or issue. Those actions are human-only.
- Do not add any agent, model, or tool to commit authorship or `Co-authored-by` annotations.
- Preserve the untracked `check_cpuid` and `check_matches` files.
- Keep the default build single-version and `transaction-cc-lockbased`.
- Keep exactly one `transaction-cc-*` feature mandatory. `transaction-mvcc` is independent and must not be added to that exactly-one group.
- Initially accept only `transaction-mvcc` plus `transaction-cc-optimistic-validation`; reject every other MVCC/CC pairing with a clear compile-time diagnostic.
- Do not modify the parser, validator, Cranelift lowering, durable record formats, recovery formats or decision rules, or non-OCC concurrency policies. Tmemory backends may expose capacity-only MVCC creation/publication helpers and a cross-build recovery restore primitive. Only creation/publication is MVCC-feature-gated; ordinary logical bounds and non-MVCC publication semantics must remain unchanged.
- Do not store MVCC history as `ObjectTable` slots or assign historical payloads an `ObjectId`.
- Do not make persistent object GC trace historical MVCC versions. Version pruning belongs to MVCC.
- Do not put durable I/O or current-state installation under a global publication mutex.
- Keep the certification permit from validation through durable/current-state installation and the atomic commit-record transition.
- A pre-LP failure must restore installed current values and abort the pending record. A post-LP failure must finish current-state installation and commit the record before returning an error.
- Before every `git add`, inspect the named paths and stage only changes belonging to this plan. Never use a repository- or directory-wide add.
- Release the certification-authority metadata mutex before reading sidecars. Hold a domain-sidecar mutex only across a typed chain lookup plus its current-value fallback or predecessor capture. Never hold the coordinator, certification-authority, or domain-sidecar metadata mutex across durable I/O.

---

## File Structure

### Create

- `crates/wasmtime/src/runtime/transaction/visibility.rs`
  - Compile-selected `TransactionVisibility` facade, always-available timestamp types, `SingleVersionVisibility`, selected type aliases, and common read/preparation contracts.
- `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`
  - MVCC module wiring and `MvccVisibility`/`MvccRuntime` exports.
- `crates/wasmtime/src/runtime/transaction/mvcc/version_chain.rs`
  - Atomic `CommitRecord`, `Version<T>`, and `VersionChain<T>`.
- `crates/wasmtime/src/runtime/transaction/mvcc/coordinator.rs`
  - Snapshot registration, visible clock, pending-commit accounting, GC begin barrier, and atomic visibility publication.
- `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
  - Six typed version tables, typed prepared values, baseline capture, reads, validation metadata, rollback predecessors, pruning, and rebase.
- `crates/wasmtime/src/runtime/transaction/mvcc/gc.rs`
  - Opportunistic version collector, current-state-only GC compatibility barrier, rebase, and restricted `MvccGcView`.
- `crates/wasmtime/src/runtime/transaction/mvcc/model.rs`
  - Test-only serializable reference model for generated schedules.
- `crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs`
  - Commit-time shared/exclusive latch authority and MVCC certification permit for optimistic validation.

### Modify

- `Cargo.toml`
  - Forward the root `transaction-mvcc` feature.
- `crates/wasmtime/Cargo.toml`
  - Declare `transaction-mvcc` without changing defaults.
- `crates/wasmtime/src/runtime/transaction/config.rs`
  - Diagnose unsupported MVCC/CC feature pairings while leaving CC selection unchanged.
- `crates/wasmtime/src/runtime/transaction/mod.rs`
  - Wire the visibility facade and feature-gated MVCC modules.
- `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`
  - Expose MVCC certification only for the optimistic pairing.
- `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs`
  - Own the feature-gated certification authority without changing single-version OCC behavior.
- `crates/wasmtime/src/runtime/transaction/state.rs`
  - Store snapshot registration in each workspace, record lock-free MVCC access sets, and own local visibility/prepared-commit state.
- `crates/wasmtime/src/runtime/transaction/granule.rs`
  - Define the always-available `TableGranuleSnapshot` and expose the existing granule snapshot shapes needed by typed visibility without changing conflict granularity.
- `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
  - Own shared visibility histories, clocks, certification latches, and pluggable persistent-GC policy.
- `crates/wasmtime/src/runtime/transaction/object_table.rs`
  - Add current-payload snapshot/install hooks keyed by logical `ObjectId`.
- `crates/wasmtime/src/runtime/transaction/object_gc.rs`
  - Define `GcMvccMode`, `TransactionPersistentGc`, and the current collector policy.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`
  - Route existing read boundaries through visibility and add the MVCC prepare/certify/publish/rollback commit path.
- `crates/wasmtime/src/runtime/transaction/tests.rs`
  - Add focused unit, concurrency, all-domain, GC, fault, and multi-store coverage.
- `crates/wasmtime/tests/transaction_persistence.rs`
  - Add file-backed restart tests proving history is volatile and latest state becomes a new baseline.

---

## Shared Contracts

Use these names consistently throughout the implementation. Define
`CommitTimestamp` and `BASELINE_TIMESTAMP` in always-compiled `visibility.rs`,
`CommitState` in feature-gated `mvcc/version_chain.rs`, `CertificationMode` in
feature-gated `concurrency/mvcc_optimistic.rs`, and `GcMvccMode` in
always-compiled `object_gc.rs`.

```rust
pub(crate) type CommitTimestamp = u64;
pub(crate) const BASELINE_TIMESTAMP: CommitTimestamp = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommitState {
    Pending,
    Committed(CommitTimestamp),
    Aborted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CertificationMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GcMvccMode {
    CurrentStateOnly,
    SnapshotAware,
}
```

The selected visibility facade has one associated snapshot-registration type and typed methods for the six domains. Reads accept a fallback closure so the MVCC implementation can hold its short metadata guard across the no-chain/current-state read.

```rust
pub(crate) trait TransactionVisibility: Clone + core::fmt::Debug + Default {
    type Snapshot: core::fmt::Debug;

    fn begin_snapshot(&self) -> Result<Self::Snapshot>;
    fn snapshot_timestamp(snapshot: &Self::Snapshot) -> CommitTimestamp;
    fn finish_snapshot(&self, snapshot: Self::Snapshot) -> Result<()>;

    fn read_memory<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>;

    fn read_memory_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>;

    fn read_global<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<GlobalSnapshot>
    where
        F: FnOnce() -> Result<GlobalSnapshot>;

    fn read_table<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<TableGranuleSnapshot>
    where
        F: FnOnce() -> Result<TableGranuleSnapshot>;

    fn read_table_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>;

    fn read_object<F>(
        &self,
        snapshot: CommitTimestamp,
        object: ObjectId,
        current: F,
    ) -> Result<Option<ObjectPayload>>
    where
        F: FnOnce() -> Result<Option<ObjectPayload>>;
}
```

`SingleVersionVisibility` implements this contract as a zero-allocation pass-through. Its snapshot type is a zero-sized value, every read calls `current`, and finish is a no-op. `MvccVisibility` implements it with `Arc<MvccRuntime>`.

`TableGranuleSnapshot` lives in always-compiled `granule.rs` because it appears
in the common facade. `MvccRuntime` itself remains feature-gated and contains
only visibility state:

```rust
#[derive(Debug, Default)]
pub(crate) struct MvccRuntime {
    coordinator: MvccCoordinator,
    domains: MvccDomains,
}
```

Certification authority remains in the selected OCC policy, not in this
runtime.

The commit path uses typed batches rather than a cross-domain value enum:

```rust
#[derive(Clone, Debug, Default)]
pub(crate) struct PreparedDomainValues {
    pub(crate) memories: BTreeMap<GranuleId, PreparedValue<Vec<u8>>>,
    pub(crate) memory_sizes: BTreeMap<GranuleId, PreparedValue<u64>>,
    pub(crate) globals: BTreeMap<GranuleId, PreparedValue<GlobalSnapshot>>,
    pub(crate) tables: BTreeMap<GranuleId, PreparedValue<TableGranuleSnapshot>>,
    pub(crate) table_sizes: BTreeMap<GranuleId, PreparedValue<u64>>,
    pub(crate) objects: BTreeMap<ObjectId, PreparedObjectValue>,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedValue<T> {
    pub(crate) predecessor: T,
    pub(crate) value: T,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedObjectValue {
    pub(crate) predecessor: Option<ObjectPayload>,
    pub(crate) value: ObjectPayload,
}
```

The absence of an object predecessor represents a newly promoted logical object. It does not create a baseline object version.

---

### Task 1: Establish the Independent Feature Axis

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/wasmtime/Cargo.toml`
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mod.rs`
- Create: `crates/wasmtime/src/runtime/transaction/visibility.rs`
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add compile-selection tests before implementation**

Add a feature-gated unit test that asserts MVCC does not become a `ConcurrencyControl` variant:

```rust
#[cfg(feature = "transaction-mvcc")]
#[test]
fn mvcc_is_visibility_not_concurrency_control() {
    assert_eq!(
        ConcurrencyControl::default_for_build(),
        ConcurrencyControl::OptimisticValidation
    );
    let visibility = SelectedTransactionVisibility::default();
    assert!(visibility.is_multiversion());
}
```

- [ ] **Step 2: Run the supported build and observe failure**

```bash
cargo check -p wasmtime
cargo check -p wasmtime --no-default-features \
  --features "anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-optimistic-validation"
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_is_visibility_not_concurrency_control --lib -- --exact
```

Expected: Cargo reports that `transaction-mvcc` is not a declared feature, or compilation reports that `SelectedTransactionVisibility` does not exist.

- [ ] **Step 3: Declare but do not default-enable `transaction-mvcc`**

In `crates/wasmtime/Cargo.toml`:

```toml
transaction-mvcc = ["transaction"]
```

In the root `Cargo.toml`:

```toml
transaction-mvcc = ["wasmtime/transaction-mvcc"]
```

Do not add it to either default feature list.

- [ ] **Step 4: Add the unsupported-pairing diagnostic**

Append this guard after the existing exactly-one-CC guards in `config.rs`:

```rust
#[cfg(all(
    feature = "transaction-mvcc",
    not(feature = "transaction-cc-optimistic-validation")
))]
compile_error!(
    "transaction-mvcc currently requires \
     transaction-cc-optimistic-validation; MVCC is an independent \
     visibility feature and other concurrency-control adapters are not yet implemented"
);
```

Do not add `transaction-mvcc` to `ConcurrencyControl`.

- [ ] **Step 5: Add the facade shell**

Implement `SingleVersionVisibility`, `TransactionVisibility`, and:

```rust
#[cfg(not(feature = "transaction-mvcc"))]
pub(crate) type SelectedTransactionVisibility = SingleVersionVisibility;

#[cfg(feature = "transaction-mvcc")]
pub(crate) type SelectedTransactionVisibility = super::mvcc::MvccVisibility;

pub(crate) type SelectedVisibilitySnapshot =
    <SelectedTransactionVisibility as TransactionVisibility>::Snapshot;
```

Give both implementations:

```rust
pub(crate) const fn is_multiversion(&self) -> bool
```

returning `false` and `true`, respectively.

- [ ] **Step 6: Verify supported and rejected configurations**

```bash
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation"
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-lockbased"
```

Expected: the first command succeeds. The second command fails and includes `transaction-mvcc currently requires transaction-cc-optimistic-validation`.

- [ ] **Step 7: Verify the default did not change**

```bash
cargo test -p wasmtime mvcc_is_visibility_not_concurrency_control --lib -- --exact
```

Expected: zero matching tests in the default non-MVCC build, with a successful test binary.

- [ ] **Step 8: Commit the feature shell**

```bash
git add Cargo.toml \
  crates/wasmtime/Cargo.toml \
  crates/wasmtime/src/runtime/transaction/config.rs \
  crates/wasmtime/src/runtime/transaction/mod.rs \
  crates/wasmtime/src/runtime/transaction/visibility.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/mod.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add transaction MVCC feature axis"
```

---

### Task 2: Implement Atomic Commit Records and Generic Version Chains

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/version_chain.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`

- [ ] **Step 1: Write version-chain tests**

Place these tests in `version_chain.rs`:

```rust
#[test]
fn pending_and_aborted_versions_are_invisible() {
    let baseline = Arc::new(CommitRecord::committed_for_baseline(0).unwrap());
    let pending = Arc::new(CommitRecord::pending());
    let aborted = Arc::new(CommitRecord::pending());
    let mut chain = VersionChain::new_baseline(baseline, 10_u64);
    chain.push_newest(pending, 20);
    chain.push_newest(aborted.clone(), 30);
    aborted.abort().unwrap();

    assert_eq!(chain.read_visible(100), Some(&10));
}

#[test]
fn one_commit_record_exposes_multiple_chains_atomically() {
    let baseline = Arc::new(CommitRecord::committed_for_baseline(0).unwrap());
    let commit = Arc::new(CommitRecord::pending());
    let mut left = VersionChain::new_baseline(baseline.clone(), 1_u64);
    let mut right = VersionChain::new_baseline(baseline, 2_u64);
    left.push_newest(commit.clone(), 11);
    right.push_newest(commit.clone(), 22);

    assert_eq!(left.read_visible(1), Some(&1));
    assert_eq!(right.read_visible(1), Some(&2));
    commit.commit(1).unwrap();
    assert_eq!(left.read_visible(1), Some(&11));
    assert_eq!(right.read_visible(1), Some(&22));
}

#[test]
fn horizon_pruning_keeps_one_predecessor_and_all_newer_commits() {
    let mut chain = VersionChain::new_baseline(
        Arc::new(CommitRecord::committed_for_baseline(0).unwrap()),
        0_u64,
    );
    for timestamp in 1..=5 {
        chain.push_newest(
            Arc::new(CommitRecord::committed_for_baseline(timestamp).unwrap()),
            timestamp,
        );
    }

    chain.prune_for_oldest_snapshot(Some(3));
    assert_eq!(chain.committed_timestamps(), vec![5, 4, 3]);
    assert_eq!(chain.read_visible(3), Some(&3));
}
```

- [ ] **Step 2: Run and observe missing types**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::version_chain::tests --lib -- --format terse
```

Expected: compilation fails because `CommitRecord` and `VersionChain` are not implemented.

- [ ] **Step 3: Implement the atomic state encoding**

Use `AtomicU64` with:

```rust
const PENDING_STATE: u64 = 0;
const ABORTED_STATE: u64 = u64::MAX;
```

Committed timestamps occupy `1..u64::MAX`. `committed_for_baseline(0)` is allowed only for immutable baseline records and stores a separate baseline flag; normal `commit` rejects timestamp zero and `u64::MAX`. All state reads use `Ordering::Acquire`, and the `Pending -> Committed` / `Pending -> Aborted` compare-exchanges use `Ordering::AcqRel`.

Required methods:

```rust
impl CommitRecord {
    pub(crate) fn pending() -> Self;
    pub(crate) fn committed_for_baseline(timestamp: CommitTimestamp) -> Result<Self>;
    pub(crate) fn state(&self) -> CommitState;
    pub(crate) fn commit(&self, timestamp: CommitTimestamp) -> Result<()>;
    pub(crate) fn abort(&self) -> Result<()>;
}
```

Reject every second transition with an error containing `commit record is no longer pending`.

- [ ] **Step 4: Implement newest-first chains**

Use:

```rust
#[derive(Clone, Debug)]
pub(crate) struct Version<T> {
    pub(crate) commit: Arc<CommitRecord>,
    pub(crate) value: T,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VersionChain<T> {
    versions: VecDeque<Version<T>>,
}
```

Required behavior:

- `new_baseline` inserts one committed version.
- `new_pending` supports newly created object identities with no baseline.
- `push_newest` uses `push_front`.
- `read_visible(snapshot)` skips pending, aborted, and committed timestamps newer than `snapshot`.
- `newest_committed_timestamp` skips pending and aborted.
- `committed_predecessor` returns the newest committed payload.
- pruning removes aborted entries immediately, preserves pending entries, keeps all committed entries newer than the oldest active snapshot, and keeps the first committed entry at or before it.

- [ ] **Step 5: Run the focused tests**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::version_chain::tests --lib -- --format terse
```

Expected: all version-chain tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/mvcc
git commit -m "Add MVCC commit records and version chains"
```

---

### Task 3: Add the Visibility Clock, Snapshot Registry, and GC Begin Barrier

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/coordinator.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`

- [ ] **Step 1: Write coordinator tests**

Add tests for:

- begin captures and registers the visible timestamp under one coordinator lock;
- dropping or explicitly finishing a registration decrements the correct multiset count exactly once;
- two registrations at the same timestamp are counted separately;
- final publication allocates consecutive timestamps with no timestamp assignment before commit;
- a GC begin barrier rejects active snapshots and pending commits;
- while the GC barrier is held, `begin_snapshot` fails with `persistent GC is active`;
- a prepared commit cannot abort after its record was committed.

Use this atomic-publication assertion:

```rust
#[test]
fn coordinator_assigns_timestamp_at_visibility_transition() {
    let coordinator = MvccCoordinator::default();
    let commit = Arc::new(CommitRecord::pending());
    let prepared = coordinator.register_pending_commit(commit.clone()).unwrap();

    assert_eq!(coordinator.visible_timestamp().unwrap(), 0);
    assert_eq!(commit.state(), CommitState::Pending);
    let timestamp = prepared.publish().unwrap();
    assert_eq!(timestamp, 1);
    assert_eq!(commit.state(), CommitState::Committed(1));
    assert_eq!(coordinator.visible_timestamp().unwrap(), 1);
}
```

- [ ] **Step 2: Run and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::coordinator::tests --lib -- --format terse
```

Expected: compilation fails because `MvccCoordinator` does not exist.

- [ ] **Step 3: Implement one lifecycle/publication mutex**

Use:

```rust
#[derive(Debug, Default)]
struct CoordinatorState {
    visible_timestamp: CommitTimestamp,
    baseline_timestamp: CommitTimestamp,
    active_snapshots: BTreeMap<CommitTimestamp, usize>,
    pending_commits: usize,
    gc_active: bool,
}

#[derive(Debug, Default)]
pub(crate) struct MvccCoordinator {
    state: Arc<Mutex<CoordinatorState>>,
}
```

`begin_snapshot` holds the mutex while checking `gc_active`, reading `visible_timestamp`, and incrementing the multiset.

`register_pending_commit` increments `pending_commits` and returns a non-cloneable `PendingCommitRegistration`.

`PendingCommitRegistration::publish` holds the same mutex, checks the record is pending, increments the timestamp with overflow checking, commits the record, updates `visible_timestamp`, decrements pending count, and consumes the registration.

`PendingCommitRegistration::abort` marks the record aborted, decrements pending count, and consumes the registration. Its `Drop` performs the same abort best-effort only while the record is still pending.

- [ ] **Step 4: Implement non-cloneable snapshot ownership**

`SnapshotRegistration` stores the timestamp and coordinator handle, has no `Clone` implementation, and unregisters on `finish` or `Drop`. This makes workspace movement, rather than reference counting, the ownership model.

- [ ] **Step 5: Implement the GC barrier permit**

`begin_gc_barrier` must atomically require:

```rust
ensure!(!state.gc_active, "persistent GC is already active");
ensure!(state.active_snapshots.is_empty(), "MVCC snapshots are active");
ensure!(state.pending_commits == 0, "MVCC commits are pending");
state.gc_active = true;
```

`MvccGcBarrierPermit::Drop` clears `gc_active`.

- [ ] **Step 6: Run focused tests and commit**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::coordinator::tests --lib -- --format terse
git add crates/wasmtime/src/runtime/transaction/mvcc
git commit -m "Add MVCC snapshot and publication coordinator"
```

Expected: all coordinator tests pass before committing.

---

### Task 4: Add Typed Domain Sidecars and Baseline-Safe Reads

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/visibility.rs`

- [ ] **Step 1: Write typed-domain tests**

Add tests proving:

- all six tables can store distinct payload types without a cross-domain value enum;
- a no-chain read calls the current-value closure while the domain metadata guard is held;
- a concurrent prepare cannot interleave between the no-chain check and fallback read;
- all pending versions prepared by one transaction share the same `Arc<CommitRecord>`;
- `latest_committed_timestamp` returns the runtime baseline for an unversioned granule;
- a new object chain has no value visible before its creation timestamp;
- table granules reject more than `TTABLE_GRANULE_SIZE` elements;
- memory granule payloads reject values larger than `TMEMORY_GRANULE_SIZE`.

Use barriers in the baseline race test: pause the fallback closure, start preparation on another thread, assert preparation is blocked, finish fallback, and then assert the prepared chain contains the captured baseline plus its pending version.

- [ ] **Step 2: Run and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::domains::tests --lib -- --format terse
```

Expected: compilation fails because the typed tables and `MvccRuntime` do not exist.

- [ ] **Step 3: Define typed payloads and tables**

Use:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TableGranuleSnapshot(Vec<TableElementSnapshot>);

#[derive(Debug, Default)]
struct VersionTable<T> {
    chains: BTreeMap<GranuleId, VersionChain<T>>,
    prune_cursor: Option<GranuleId>,
}

#[derive(Debug, Default)]
struct ObjectVersionTable {
    chains: BTreeMap<ObjectId, VersionChain<ObjectPayload>>,
    prune_cursor: Option<ObjectId>,
}

#[derive(Debug, Default)]
struct MvccDomainState {
    memories: VersionTable<Vec<u8>>,
    memory_sizes: VersionTable<u64>,
    globals: VersionTable<GlobalSnapshot>,
    tables: VersionTable<TableGranuleSnapshot>,
    table_sizes: VersionTable<u64>,
    objects: ObjectVersionTable,
}
```

Keep `MvccDomainState` behind one `Mutex` for the first milestone. Do not introduce `enum DomainValue`.

- [ ] **Step 4: Implement baseline-safe typed reads**

Each read method:

1. locks `MvccDomainState`;
2. checks the appropriate typed table;
3. if a chain exists, clones the visible value and returns it;
4. if no chain exists, calls the supplied current-value closure before releasing the lock.

The object method returns `Option<ObjectPayload>` because a chain for a newly created object can have no visible predecessor for an older snapshot.

- [ ] **Step 5: Implement typed commit preparation**

`MvccPrepareGuard` holds the domain mutex while the caller captures current values and appends every pending version. It owns one shared commit record and builds `PreparedDomainValues`.

Required typed methods:

```rust
pub(crate) fn prepare_memory(
    &mut self,
    granule: GranuleId,
    current: impl FnOnce() -> Result<Vec<u8>>,
    value: Vec<u8>,
) -> Result<()>;

pub(crate) fn prepare_memory_size(
    &mut self,
    granule: GranuleId,
    current: impl FnOnce() -> Result<u64>,
    value: u64,
) -> Result<()>;

pub(crate) fn prepare_global(
    &mut self,
    granule: GranuleId,
    current: impl FnOnce() -> Result<GlobalSnapshot>,
    value: GlobalSnapshot,
) -> Result<()>;

pub(crate) fn prepare_table(
    &mut self,
    granule: GranuleId,
    current: impl FnOnce() -> Result<TableGranuleSnapshot>,
    value: TableGranuleSnapshot,
) -> Result<()>;

pub(crate) fn prepare_table_size(
    &mut self,
    granule: GranuleId,
    current: impl FnOnce() -> Result<u64>,
    value: u64,
) -> Result<()>;

pub(crate) fn prepare_object(
    &mut self,
    object: ObjectId,
    current: impl FnOnce() -> Result<Option<ObjectPayload>>,
    value: ObjectPayload,
) -> Result<()>;
```

When a chain is absent and `current` returns `Some`, seed a baseline at the coordinator's `baseline_timestamp`. For a new object with `None`, create a pending-only chain.

- [ ] **Step 6: Implement access-set timestamp lookup**

Add:

```rust
pub(crate) fn latest_committed_timestamp(
    &self,
    granule: GranuleId,
) -> Result<CommitTimestamp>;
```

Dispatch on `GranuleId` only to select the typed table, not to combine payloads. Object granules map to logical `ObjectId`. Return `baseline_timestamp` for no chain.

- [ ] **Step 7: Run tests and commit**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc::domains::tests --lib -- --format terse
git add crates/wasmtime/src/runtime/transaction/mvcc \
  crates/wasmtime/src/runtime/transaction/visibility.rs
git commit -m "Add typed MVCC domain sidecars"
```

---

### Task 5: Add Commit-Time OCC Certification Latches

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write certification tests**

Add tests for:

- read-only granules receive shared reservations;
- write granules receive exclusive reservations even when also read;
- shared/shared access overlaps;
- shared/exclusive and exclusive/exclusive access waits;
- a waiting transaction acquires after the first permit drops;
- acquiring an entire sorted set atomically prevents deadlock;
- disjoint permits coexist;
- validation aborts when any read or blind-write granule has a committed timestamp newer than the snapshot;
- the single-version optimistic policy still uses its existing execution-time version checks when `transaction-mvcc` is disabled.

The write-skew-shaped latch test uses:

```rust
let first_reads = BTreeSet::from([a, b]);
let first_writes = BTreeSet::from([a]);
let second_reads = BTreeSet::from([a, b]);
let second_writes = BTreeSet::from([b]);
```

and asserts only one permit can certify at a time.

- [ ] **Step 2: Run and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_certification --lib -- --format terse
```

Expected: compilation fails because the certification adapter is absent.

- [ ] **Step 3: Implement an atomic access-set reservation table**

Use one short metadata `Mutex` plus `Condvar`; the mutex is not held after reservations are installed:

```rust
#[derive(Debug, Default)]
struct LatchState {
    readers: BTreeSet<TransactionId>,
    writer: Option<TransactionId>,
}

#[derive(Debug, Default)]
struct CertificationState {
    granules: BTreeMap<GranuleId, LatchState>,
}

#[derive(Debug, Default)]
pub(crate) struct MvccCertificationAuthority {
    state: Mutex<CertificationState>,
    changed: Condvar,
}
```

Build a sorted `Vec<(GranuleId, CertificationMode)>` from the union. Treat every write as exclusive. Under the mutex, reserve the entire set only if every entry is compatible; otherwise wait on the condition variable and recheck. This all-or-wait rule removes partial-acquisition deadlocks while retaining granule-level parallelism.

- [ ] **Step 4: Implement the RAII permit**

`MvccCertificationPermit` stores the authority `Arc`, transaction id, and exact sorted reservations. `Drop` removes the transaction from all entries, removes empty latch entries, and calls `notify_all`.

The permit must be non-cloneable and expose no early unlock method to the commit pipeline.

- [ ] **Step 5: Integrate with the selected OCC policy only**

Under:

```rust
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
```

add an `Arc<MvccCertificationAuthority>` to `OptimisticValidation` and forward:

```rust
pub(crate) fn acquire_mvcc_certification(
    &self,
    transaction: TransactionId,
    reads: &BTreeSet<GranuleId>,
    writes: &BTreeSet<GranuleId>,
) -> Result<MvccCertificationPermit>;
```

Expose it through `ConcurrencyControlState` and `TransactionRegionRuntime`. Do not add the method to non-OCC modules.

- [ ] **Step 6: Validate after reservation**

After the permit is acquired, validate every granule in the read/write union:

```rust
for granule in reads.union(writes).copied() {
    ensure!(
        visibility.latest_committed_timestamp(granule)? <= snapshot,
        "transaction MVCC certification conflict on {granule:?}"
    );
}
```

Hold the permit when returning success. Drop it before returning the conflict error.

- [ ] **Step 7: Verify MVCC and single-version OCC**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_certification --lib -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-cc-optimistic-validation" \
  optimistic_validation --lib -- --format terse
```

Expected: both focused suites pass.

- [ ] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add MVCC optimistic commit certification"
```

---

### Task 6: Attach Snapshots to Workspaces and Make MVCC Execution Lock-Free

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/visibility.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Write lifecycle tests**

Add tests proving:

- `begin` and `begin_with_region_runtime` each register exactly one snapshot;
- entering a previously unknown transaction id creates exactly one fresh snapshot;
- suspend/restore preserves the same timestamp;
- aborting an active or suspended transaction unregisters once;
- commit cleanup unregisters once;
- conflict-driven workspace discard unregisters once;
- a persistent-GC begin barrier prevents transaction begin;
- MVCC `acquire_granule_read` and `acquire_granule_write` record workspace sets without calling execution-time CC acquisition;
- non-MVCC builds retain existing policy acquisition behavior.

- [ ] **Step 2: Run the snapshot tests and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_snapshot_workspace --lib -- --format terse
```

Expected: tests fail because workspaces do not own visibility snapshots.

- [ ] **Step 3: Add selected visibility ownership**

Add to `TransactionState`:

```rust
visibility: SelectedTransactionVisibility,
visibility_snapshot: Option<SelectedVisibilitySnapshot>,
```

Add to `TransactionWorkspace`:

```rust
visibility_snapshot: Option<SelectedVisibilitySnapshot>,
```

Move the field in `take_workspace` and `install_workspace`; never clone it.

For a shared region, obtain the selected visibility handle from `TransactionRegionRuntime`. For a local transaction, use `TransactionState::visibility`.

- [ ] **Step 4: Register before activating the transaction**

In both begin paths:

1. prepare the state;
2. begin the visibility snapshot;
3. allocate the transaction id;
4. install the registration;
5. activate the transaction.

If id allocation or activation fails, finish/drop the registration before returning.

- [ ] **Step 5: Handle enter and restore without refreshing snapshots**

In `enter_transaction`, suspend the current workspace first. If the requested
transaction already has a suspended workspace, move it back unchanged. If it
has no suspended workspace, create a default workspace and register a new
snapshot before activation. `restore_transaction(Some(id))` must require and
move the suspended workspace as it does today; it must never take a new
snapshot for that id. `restore_transaction(None)` installs a default workspace
with no snapshot.

- [ ] **Step 6: Add a read context accessor**

Return a cloned facade handle and copied timestamp without cloning the registration:

```rust
#[derive(Clone, Debug)]
pub(crate) struct VisibilityReadContext {
    pub(crate) visibility: SelectedTransactionVisibility,
    pub(crate) snapshot: CommitTimestamp,
}
```

`TransactionState::active_visibility_read_context` requires an active transaction and reads the timestamp from the registration.

- [ ] **Step 7: Feature-gate execution-time CC access**

In MVCC builds, `acquire_granule_read` and `acquire_granule_write` ignore backend/current version arguments for CC purposes and only update `read_granules`/`write_granules`. Writes must also enter the read set because partial writes and staged reads consume snapshot state.

Keep the current code unchanged in the `not(feature = "transaction-mvcc")` branch.

Likewise, bypass the existing execution-time `validate_active_read` loop in the MVCC commit path; Task 5 certification replaces it.

- [ ] **Step 8: Finish the snapshot in every terminal path**

Take and finish the snapshot during:

- normal commit completion;
- active abort;
- suspended abort;
- conflict-driven discard;
- constructor-boundary failure;
- object-aware abort.

Let `Drop` remain the poison/error fallback, but assert in tests that successful terminal paths use explicit finish.

- [ ] **Step 9: Run lifecycle and existing transaction-state tests**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_snapshot_workspace --lib -- --format terse
cargo test -p wasmtime transaction:: --lib -- --format terse
```

Expected: MVCC lifecycle tests pass and the default transaction suite remains green.

- [ ] **Step 10: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/visibility.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Attach MVCC snapshots to transaction workspaces"
```

---

### Task 7: Route Tmemory, Size, Global, and Table Reads Through Visibility

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/granule.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add all-non-object-domain read tests**

Cover:

- repeated tmemory loads remain stable after another transaction commits;
- partial tmemory writes start from snapshot-visible bytes;
- cross-granule loads/stores use one snapshot;
- snapshot-visible memory size controls bounds and staged growth;
- globals retain scalar/reference snapshots and read staged values first;
- table gets read the correct 16-element snapshot granule;
- table size and staged growth use snapshot-visible size;
- table fill/copy/range operations record every data granule plus the size granule.

Name the tests with the `mvcc_visibility_` prefix.

- [ ] **Step 2: Run and observe current-state reads**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_visibility_ --lib -- --format terse
```

Expected: stable-read tests fail because helpers read current live state.

- [ ] **Step 3: Add current snapshot helpers without changing backends**

Keep `collect_tmemory_access_snapshot` unchanged as the current-state backend reader. Add libcall-level wrappers that:

1. clone `VisibilityReadContext`;
2. call the typed visibility method;
3. supply the existing current-state helper as its closure;
4. reconstruct `TMemoryAccessSnapshot` with visible granule payloads and visible size.

Do not modify `VMemory`, `FileBackedMemory`, or `DaxPmem`.

- [ ] **Step 4: Preserve staged-first reads**

For every domain:

1. acquire/record the granule;
2. return a staged value when present;
3. otherwise call the typed visibility facade.

For tmemory, merge all staged granules over the visibility-produced access snapshot using the existing `read_tmemory_owned_from_snapshot` logic.

- [ ] **Step 5: Make size reads authoritative for bounds**

Add `visible_tmemory_pages` and `visible_ttable_size` helpers. Every grow, bounds, copy, fill, and range operation must use:

```text
staged size, if present
otherwise snapshot-visible size
```

Never use a newer current size to make an older snapshot's out-of-bounds access succeed.

- [ ] **Step 6: Read whole table granules**

Add a helper that snapshots indices:

```rust
granule_start..min(granule_start + TTABLE_GRANULE_SIZE, snapshot_visible_table_size)
```

and stores them in `TableGranuleSnapshot`. Individual gets select their offset. Staged table elements overlay the visible granule.

- [ ] **Step 7: Run domain tests and default regression tests**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_visibility_ --lib -- --format terse
cargo test -p wasmtime transaction_tmemory --lib -- --format terse
cargo test -p wasmtime transaction_tglobal --lib -- --format terse
cargo test -p wasmtime transaction_ttable --lib -- --format terse
```

Expected: all commands pass.

- [ ] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/granule.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Route transaction reads through MVCC visibility"
```

---

### Task 8: Put Object Payload History Behind Logical Object IDs

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add object history tests**

Cover:

- struct and array payload reads remain stable after a newer commit;
- staged object fields/elements are read first;
- one logical `ObjectId` has one current `ObjectTable` slot regardless of history length;
- historical payloads never increase `ObjectTable::live_count`;
- an object created after a transaction's snapshot is not visible to that transaction;
- transaction-local promotion creates a pending-only object chain;
- aborting promotion removes the pending version and frees the transaction-local allocation;
- persistent object refresh still uses the shared directory for current state.

- [ ] **Step 2: Run and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_object_ --lib -- --format terse
```

Expected: old-snapshot object reads see current payloads.

- [ ] **Step 3: Add narrow current-payload hooks**

Add to `ObjectTable`:

```rust
pub(crate) fn current_payload_snapshot(
    &mut self,
    object_id: ObjectId,
) -> Result<Option<ObjectPayload>>;

pub(crate) fn install_current_payload_snapshot(
    &mut self,
    object_id: ObjectId,
    payload: &ObjectPayload,
) -> Result<()>;
```

The snapshot helper refreshes shared persistent state first and returns `None` only when the logical object does not exist. The install helper preserves kind, type-layout id, runtime type index, persistence metadata, and the existing object identity.

- [ ] **Step 4: Route `read_object_payload` through visibility**

Keep this order:

1. staged persistent object;
2. transaction-local object;
3. MVCC-visible payload behind the logical `ObjectId`;
4. missing-at-snapshot error.

Still record the existing whole-object `GranuleId`.

- [ ] **Step 5: Keep historical references out of object GC**

Add test-only accessors on the MVCC sidecar to report object chain/version counts. Assert that object GC APIs receive only `ObjectTable`, current roots, and current payloads; they receive no `Version<ObjectPayload>` and no historical edge list.

- [ ] **Step 6: Run object and existing object-GC tests**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_object_ --lib -- --format terse
cargo test -p wasmtime persistent_gc --lib -- --format terse
```

Expected: both suites pass.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_table.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/domains.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Add logical object MVCC sidecars"
```

---

### Task 9: Build Typed Commit Preparation and Atomic Mixed-Domain Visibility

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/coordinator.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add prepare/publication tests**

Cover:

- read-only commit creates no commit record and takes no certification permit;
- a writer certifies the full read/write union before changing current state;
- every written domain receives one pending version with the same commit record;
- mixed memory/global/table/object values remain invisible while the record is pending;
- one record transition exposes all domains;
- commit latches remain held until after the transition;
- two disjoint prepared commits can both be inside current/durable installation;
- transaction-local object initial versions share the same record.

- [ ] **Step 2: Run and observe failure**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_prepare_ --lib -- --format terse
```

Expected: no MVCC terminal commit pipeline exists.

- [ ] **Step 3: Split the single-version and MVCC commit paths**

Keep the existing algorithm in:

```rust
fn transaction_commit_single_version_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
) -> Result<()>;
```

Add:

```rust
#[cfg(feature = "transaction-mvcc")]
fn transaction_commit_mvcc_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
) -> Result<()>;
```

The outer `transaction_commit_impl` retains scratch flushing and structured-failure handling, then compile-selects one path. Avoid runtime `if is_multiversion()` branches in the default build.

- [ ] **Step 4: Collect complete final values before installation**

Build `PreparedDomainValues` from:

- complete staged tmemory granules;
- new memory sizes;
- globals;
- whole table granules with all staged element overlays;
- table sizes;
- staged persistent and newly promoted object payloads.

Capture every predecessor through `MvccPrepareGuard` while its domain metadata lock is held. Preserve the typed batch until all post-commit cleanup finishes; do not remove the only copy when current state is installed.

- [ ] **Step 5: Enforce commit ordering**

The MVCC writer path must execute in this order:

```text
flush helper scratch
promote references
collect read/write sets
acquire complete OCC certification permit
validate latest committed timestamps against snapshot
create one pending commit record
capture predecessors and append typed pending versions
run existing durable/current-state publication
install required mapped object/runtime state
apply committed persistent-root state
publish the commit record with the next timestamp
clear workspace and release snapshot
drop certification permit
run budgeted version pruning
```

The certification permit and pending-commit registration are local variables whose scopes cover publication. Do not store them in a global map.

- [ ] **Step 6: Keep read-only commits lock-free**

If `write_granules.is_empty()`:

- skip certification;
- skip pending record creation;
- finish at the captured snapshot timestamp;
- release the snapshot/workspace through the normal terminal cleanup path.

- [ ] **Step 7: Add an atomic visibility test hook**

Under `cfg(test)`, add barriers immediately before and after the commit-record transition. Use them to hold a mixed-domain commit pending while an observer reads, then release and assert the observer sees either all predecessors or all committed values, never a mixture.

- [ ] **Step 8: Run focused and default suites**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_prepare_ --lib -- --format terse
cargo test -p wasmtime transaction_commit --lib -- --format terse
```

Expected: both commands pass.

- [ ] **Step 9: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/mvcc \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Publish mixed-domain MVCC commits atomically"
```

---

### Task 10: Make Pre-LP Rollback and Post-LP Completion Correct

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory.rs`
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Test: `crates/wasmtime/tests/transaction_persistence.rs`

- [ ] **Step 1: Add fault-injection tests at every cut point**

Inject failures:

- after certification but before preparation;
- after preparation but before any current install;
- after each typed domain's first current install;
- after durable data publication but before LP;
- immediately before LP using the existing hook;
- immediately after LP but before mapped-object install;
- during persistent-root apply after LP;
- during cleanup after the commit-record transition.

For every pre-LP failure assert:

- commit record is `Aborted`;
- pending/aborted versions are invisible;
- every physically installed value equals its predecessor;
- latches and snapshot registration are released;
- promoted/new objects are cleaned up;
- a later GC rebase cannot adopt the failed value.

For every post-LP failure assert:

- all final physical values are installed;
- the record becomes `Committed`;
- a new transaction sees all final values;
- the returned error states that the transaction committed durably but cleanup failed.

- [ ] **Step 2: Run and observe rollback failures**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_fault_ --lib -- --format terse
```

Expected: pre-LP current-state mutations are not yet restored or post-LP records remain pending.

- [ ] **Step 3: Track typed installation progress**

Add typed installed-key sets to the prepared commit:

```rust
#[derive(Debug, Default)]
pub(crate) struct InstalledDomainKeys {
    memories: BTreeSet<GranuleId>,
    memory_sizes: BTreeSet<GranuleId>,
    globals: BTreeSet<GranuleId>,
    tables: BTreeSet<GranuleId>,
    table_sizes: BTreeSet<GranuleId>,
    objects: BTreeSet<ObjectId>,
}
```

Record a key only after its current-state install succeeds.

- [ ] **Step 4: Implement pre-LP rollback**

On a pre-LP error:

1. restore installed keys in reverse domain/publication order from `PreparedDomainValues::predecessor`;
2. remove a newly created object when its predecessor is `None`;
3. mark the commit record aborted;
4. remove aborted chain heads where no rollback depends on them;
5. invoke existing durable loose-end cleanup and object-aware abort;
6. release the snapshot and certification permit.

If rollback itself fails, return the rollback error with the original error as context and retain enough typed state for an explicit retry; do not rebase.

- [ ] **Step 5: Implement post-LP force completion**

Once `publish_commit_lp` succeeds, set `lp_published = true`. Every later error enters a force-completion routine that:

1. installs/reinstalls every final typed value idempotently;
2. installs mapped object publications;
3. applies committed root metadata;
4. publishes the pending commit record;
5. clears workspace/snapshot and drops latches;
6. returns the original cleanup error, enriched with any completion error.

Never call `CommitRecord::abort` on this path.

- [ ] **Step 6: Preserve staged data until the decision**

Refactor `commit_staged_tmemory_records` and `apply_staged_transaction_record` so MVCC installation does not erase the only staged/final copy. The single-version path may retain its existing removal timing.

- [ ] **Step 7: Verify live faults and durable recovery**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_fault_ --lib -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence mvcc_file_backed_lp_
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  ordinary_logical_bounds_reject_reserved_capacity_addresses --lib
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-cc-optimistic-validation" \
  ordinary_logical_bounds_reject_reserved_capacity_addresses --lib
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-cc-optimistic-validation" \
  file_backed_recovery_restores_hidden_growth_tail_without_mvcc_feature --lib
```

Expected: all pre/post-LP and restart assertions pass. Both MVCC and non-MVCC
builds reject ordinary access beyond the committed logical length, and the
non-MVCC recovery build can restore a loose growth-tail undo produced by an
MVCC build. No durable record-format change is involved.

- [ ] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/object_table.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/domains.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Handle MVCC publication failures around commit LP"
```

---

### Task 11: Prevent Write Skew and Prove Serializable Schedules

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/model.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add deterministic schedule tests**

Use barriers to construct:

- blind write/write conflict;
- lost update;
- classic two-record write skew;
- a writer with a changed read dependency and a disjoint write;
- read-only transaction concurrent with a writer;
- disjoint writing transactions overlapping terminal publication;
- multi-store conflicting and disjoint schedules.

For write skew initialize `A = 1`, `B = 1`. Both transactions read both values. The first stages `A = 0`; the second stages `B = 0`. Release both commits concurrently. Assert exactly one commits, the other reports an MVCC certification conflict, and `A + B >= 1`.

- [ ] **Step 2: Run and verify the write-skew regression first**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_serializable_write_skew_aborts_one_writer --lib -- --exact --nocapture
```

Expected: the test passes and preserves the invariant.

- [ ] **Step 3: Add a small reference model**

The test-only model tracks:

```rust
#[derive(Clone, Debug)]
struct ModelTransaction {
    snapshot: CommitTimestamp,
    reads: BTreeSet<GranuleId>,
    writes: BTreeMap<GranuleId, i64>,
}

#[derive(Debug, Default)]
struct SerializableMvccModel {
    visible: CommitTimestamp,
    versions: BTreeMap<GranuleId, Vec<(CommitTimestamp, i64)>>,
}
```

Its commit rule is the implementation rule: reject if any read/write granule's newest committed timestamp exceeds the snapshot; otherwise assign the next timestamp and append all writes.

- [ ] **Step 4: Compare generated schedules**

Generate bounded schedules from a fixed deterministic seed. For 2–4 granules and 2–4 transactions:

- produce begin/read/write/commit interleavings;
- execute them against the model;
- execute equivalent operations through test visibility/certification helpers;
- compare commit/abort outcomes and visible final values;
- assert every committed outcome has a serial order accepted by the model.

Print the seed and operation list on failure.

- [ ] **Step 5: Run the schedule suite repeatedly**

```bash
for run in 1 2 3 4 5
do
  cargo test -p wasmtime --no-default-features \
    --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
    mvcc_serializable_ --lib -- --format terse
done
```

Expected: all five runs pass without deadlock.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/mvcc \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Test serializable MVCC schedules"
```

---

### Task 12: Make Persistent GC Pluggable and Add the MVCC Compatibility Barrier

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/object_gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Create: `crates/wasmtime/src/runtime/transaction/mvcc/gc.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/mvcc/domains.rs`
- Test: `crates/wasmtime/src/runtime/transaction/tests.rs`

- [ ] **Step 1: Add GC policy and barrier tests**

Cover:

- the current collector advertises `CurrentStateOnly`;
- policy selection is independent from MVCC, CC, and backend configuration;
- current-state GC rejects an active or suspended snapshot;
- current-state GC rejects a prepared pending commit;
- once admitted, the barrier prevents new transaction begin;
- a complete rebase occurs before the collector callback;
- the collector sees only current `ObjectTable` identities/payloads;
- a mock `SnapshotAware` policy receives `MvccGcView`, not raw sidecar mutation access;
- a non-MVCC build invokes the current collector directly;
- incremental state still restarts after an intervening persistent-GC epoch.

- [ ] **Step 2: Run and observe missing policy interface**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_gc_ --lib -- --format terse
```

Expected: compilation fails because `TransactionPersistentGc` and the adapter do not exist.

- [ ] **Step 3: Define the independent persistent-GC policy**

In `object_gc.rs`, define an object-safe policy surface for every existing GC
entry point:

```rust
pub(crate) trait TransactionPersistentGc:
    core::fmt::Debug + Send + Sync + 'static
{
    fn mvcc_mode(&self) -> GcMvccMode;

    fn mark_sweep(
        &self,
        view: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<PersistentMarkSweepReport>;

    fn maintenance_step(
        &self,
        view: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        budget: PersistentGcBudget,
    ) -> Result<PersistentGcStepReport>;

    fn finish_cycle(
        &self,
        view: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
    ) -> Result<Option<PersistentMarkSweepReport>>;

    fn compact(
        &self,
        view: Option<&dyn TransactionMvccGcView>,
        state: &mut TransactionState,
        objects: &mut ObjectTable,
        recovery_report: &PersistentRecoveryGcReport,
    ) -> Result<PersistentObjectCompactionReport>;
}
```

Define the restricted view contract independently from the MVCC feature:

```rust
pub(crate) trait TransactionMvccGcView {
    fn oldest_active_snapshot(&self) -> Option<CommitTimestamp>;

    fn visit_retained_object_payloads(
        &self,
        visitor: &mut dyn FnMut(ObjectId, &ObjectPayload) -> Result<()>,
    ) -> Result<()>;
}
```

Extract the bodies of the four current `TransactionState` GC methods into
private `*_uncoordinated` methods. `CurrentStatePersistentGc` delegates to
those methods and requires `view.is_none()`. A future snapshot-aware policy can
consume `Some(view)` without receiving mutable chains or synthetic object
identities.

Store an `Arc<dyn TransactionPersistentGc>` in `TransactionRegionRuntimeInner` and a default policy handle in local `TransactionState`. Add test-only injection on the runtime. Do not infer the policy from a Cargo feature or `TMemoryBackend`.

- [ ] **Step 4: Add the restricted snapshot-aware view**

Implement `TransactionMvccGcView` for a concrete view that exposes only:

```rust
pub(crate) struct MvccGcView<'a> {
    oldest_active_snapshot: Option<CommitTimestamp>,
    domain_state: MutexGuard<'a, MvccDomainState>,
}
```

Its visitor walks retained object payloads behind their real logical
`ObjectId`s. It does not expose `ObjectTableSlot`, mutable chains, commit
transitions, or synthetic object ids.

- [ ] **Step 5: Implement budgeted MVCC pruning**

After transaction completion, scan a bounded number of chains using per-table cursors. With oldest active snapshot `h`, retain pending entries, every committed entry newer than `h`, and the newest committed entry at or before `h`. Remove aborted entries when rollback is complete.

With no active snapshot, retain only the newest committed state required until full rebase.

- [ ] **Step 6: Implement current-state-only barrier entry**

In an MVCC build:

1. acquire `MvccGcBarrierPermit`, which proves zero snapshots and pending commits and blocks begin;
2. acquire the existing `PersistentGcRegionPermit`, which excludes user commits;
3. verify every current domain payload equals its newest committed sidecar payload;
4. clear all sidecar histories and obsolete records;
5. set `baseline_timestamp = visible_timestamp`;
6. retire unused OCC latch metadata;
7. invoke the current collector.

Acquire permits in this order everywhere to avoid inversion. Drop them in reverse order.

In a non-MVCC build, call the policy directly through the existing GC coordination permit.

- [ ] **Step 7: Handle incremental entry points**

Apply the same adapter to:

- full `persistent_mark_sweep_collect`;
- `persistent_gc_maintenance_step`;
- `finish_persistent_gc_cycle_and_sweep`;
- persistent object compaction entry points.

Each incremental call may independently enter and rebase because the active-snapshot requirement is checked on every step. Preserve existing epoch invalidation.

- [ ] **Step 8: Run GC, object, and default suites**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_gc_ --lib -- --format terse
cargo test -p wasmtime persistent_gc --lib -- --format terse
cargo test -p wasmtime transaction:: --lib -- --format terse
```

Expected: all commands pass and the mock snapshot-aware probe cannot mutate sidecars.

- [ ] **Step 9: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/object_gc.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/gc.rs \
  crates/wasmtime/src/runtime/transaction/mvcc/domains.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Adapt pluggable persistent GC to MVCC"
```

---

### Task 13: Verify Volatile History Across All Storage Backends and Restart

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Modify: `crates/wasmtime/tests/transaction_persistence.rs`

- [ ] **Step 1: Add backend-independent setup coverage**

For each supported `TMemoryBackend`, assert that `transaction-mvcc` changes only visibility:

- VMemory uses the same current-state API;
- file-backed memory uses the same undo/data/LP records;
- DAX PMEM uses the same research/required persistence modes;
- no backend owns or serializes a version chain.

- [ ] **Step 2: Add restart tests**

For file-backed and research-mode DAX:

1. commit at least two versions;
2. confirm an old live snapshot reads the predecessor before shutdown;
3. drop the runtime;
4. recover using the existing recovery path;
5. begin a new transaction;
6. assert the latest durable state is visible;
7. assert the new runtime visible/baseline timestamp starts at zero;
8. assert no historical sidecar entries or old snapshot registrations exist.

Also cover crash-before-LP and crash-after-LP using existing recovery fault hooks.

- [ ] **Step 3: Run VMemory and file-backed suites**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_backend_ --lib -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence mvcc_
```

Expected: both suites pass.

- [ ] **Step 4: Run research-mode DAX tests on supported hosts**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_dax_research_ --lib -- --format terse
```

Expected: research-mode DAX tests pass without requiring real PMEM. Hardware-required tests remain ignored unless the host is explicitly configured.

- [ ] **Step 5: Prove durable formats did not change**

```bash
git diff ca6090dca -- \
  crates/wasmtime/src/runtime/transaction/persist.rs \
  crates/wasmtime/src/runtime/vm/block_region \
  crates/wasmtime/src/runtime/vm/memory/tmemory
```

Expected: no diff in durable record or recovery formats. The approved
capacity-only helper diff may appear in tmemory/backend files, provided it is
used only by MVCC publication or cross-build undo recovery and ordinary
logical-length APIs remain unchanged.

- [ ] **Step 6: Commit tests**

```bash
git add crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Test MVCC across transaction storage backends"
```

---

### Task 14: Run the Complete Feature and Correctness Matrix

**Files:**
- Verification only. Any corrective edit must stay within a path listed in the File Structure section.

- [ ] **Step 1: Format and check whitespace**

```bash
cargo fmt --all
cargo fmt --all -- --check
git diff --check
```

Expected: all commands succeed.

- [ ] **Step 2: Run the default non-MVCC transaction suite**

```bash
cargo test -p wasmtime transaction:: --lib -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
```

Expected: default lock-based behavior passes unchanged.

- [ ] **Step 3: Run every existing single-version CC build**

```bash
for feature in \
  transaction-cc-lockbased \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering
do
  cargo test -p wasmtime --no-default-features \
    --features "runtime,std,gc,cranelift,wat,${feature}" \
    transaction:: --lib -- --format terse
done
```

Expected: all seven builds compile and pass without MVCC.

- [ ] **Step 4: Run the supported MVCC build**

```bash
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction:: --lib -- --format terse
cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence -- --format terse
```

Expected: the full MVCC unit and persistence suites pass.

- [ ] **Step 5: Check invalid build diagnostics**

```bash
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc"
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-mvcc,transaction-cc-lockbased"
cargo check -p wasmtime --no-default-features \
  --features "runtime,std,gc,transaction-cc-lockbased,transaction-cc-optimistic-validation"
```

Expected:

- MVCC with no CC fails with the existing exactly-one requirement;
- MVCC plus lock-based fails with the unsupported adapter diagnostic;
- two CC features fail with the existing exactly-one diagnostic.

- [ ] **Step 6: Run targeted concurrency tests under a timeout**

```bash
timeout 180 cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_serializable_ --lib -- --nocapture
timeout 180 cargo test -p wasmtime --no-default-features \
  --features "runtime,std,gc,cranelift,wat,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_prepare_ --lib -- --nocapture
```

Expected: both commands finish within 180 seconds without deadlock.

- [ ] **Step 7: Inspect the constrained edit surface**

```bash
git diff --name-only ca6090dca
git diff --stat ca6090dca
```

Expected: changes are confined to the feature declarations, transaction runtime modules, transaction libcalls, and transaction tests listed by this plan. Investigate any parser, validator, Cranelift, backend, durable-format, or recovery-format change.

- [ ] **Step 8: Review acceptance criteria explicitly**

Confirm from test output:

- write skew aborts at least one participant;
- pending and mixed-domain partial commits are never visible;
- disjoint terminal publications overlap;
- read-only transactions take no latches;
- GC cannot enter with active snapshots or prepared commits;
- current-state GC sees no historical versions;
- history disappears on rebase and restart;
- all backends remain runtime-selectable in the MVCC+OCC build;
- the default and every non-MVCC CC build remain unchanged.

- [ ] **Step 9: Commit final verification-only adjustments**

If verification required code or test adjustments:

```bash
git add Cargo.toml \
  crates/wasmtime/Cargo.toml \
  crates/wasmtime/src/runtime/transaction/config.rs \
  crates/wasmtime/src/runtime/transaction/mod.rs \
  crates/wasmtime/src/runtime/transaction/visibility.rs \
  crates/wasmtime/src/runtime/transaction/mvcc \
  crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/granule.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/object_table.rs \
  crates/wasmtime/src/runtime/transaction/object_gc.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/tests/transaction_persistence.rs
git commit -m "Complete serializable MVCC verification"
```

If no adjustment was required, do not create an empty commit.

---

## Final Human Review Gate

Before any upstream action, a human should review:

- the feature-axis diagnostics;
- certification latch lifetime and lock ordering;
- complete read/write-set validation;
- mixed-domain visibility transition;
- every pre/post-LP fault path;
- object history isolation from `ObjectTable` and persistent GC;
- current-state GC barrier and full rebase;
- backend/recovery non-changes;
- the full build matrix.

Opening, reviewing, commenting on, or otherwise interacting with a Wasmtime pull request remains a human-only action under the Bytecode Alliance AI Tool Use Policy.
