# Single-Version OCC Commit Certification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent write skew in single-version optimistic validation by holding an atomic per-granule read/write certification permit from version revalidation through terminal cleanup, without serializing disjoint commits.

**Architecture:** Generalize the existing MVCC optimistic certification authority so it is compiled and owned by every optimistic-validation build. Route one RAII permit through the shared or local concurrency authority, acquire the complete access set after it is finalized and before any commit-time version validation, and retain the permit through durable/current-state installation and cleanup in the real `tfunc`, direct state, and benchmark commit paths.

**Tech Stack:** Rust 2024, Wasmtime crate-private transaction concurrency APIs, `Arc<Mutex<_>>` plus `Condvar`, existing transaction feature matrix, deterministic Rust unit/integration tests.

## Global Constraints

- Fix the optimistic policy itself; do not serialize or special-case the benchmark schedule.
- Do not add a global commit mutex. Disjoint OCC access sets must certify concurrently.
- A read-only granule receives a shared reservation; any written granule receives an exclusive reservation.
- Reserve the complete sorted access set atomically. Do not acquire per-granule latches incrementally.
- Finalize the transaction access sets before certification, revalidate every recorded read and write while certified, and hold the permit through terminal cleanup.
- No physical or durable write is allowed before certification and all version validation succeed.
- Preserve existing conflict diagnostics used by retry classification.
- Shared-region transactions use the region's shared authority; standalone transaction states use their local selected policy authority.
- Serializable MVCC retains certification followed by snapshot-timestamp validation and retains its existing commit/rollback behavior.
- Other compile-time concurrency-control policies do not acquire this permit and retain their current behavior.
- Preserve VMemory, file-backed copy-on-write durability, runtime-selectable storage, and pluggable GC behavior.
- Keep the uncommitted benchmark Task 5 draft intact. OCC commits stage only the files named by this plan; do not discard, overwrite, or accidentally include the draft in an OCC commit.
- No public API, new Cargo feature, runtime-selectable CC axis, performance threshold, or unrelated refactor.
- Do not open, comment on, or review a Wasmtime pull request or issue. Do not push or add an agent, model, or tool as a commit co-author.

## Complete Feature Closure

Use this baseline for exact non-default builds:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser
```

Append `transaction-cc-optimistic-validation` for single-version OCC. Append
`transaction-mvcc,transaction-cc-optimistic-validation` for MVCC.

## File Structure

- Create `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_certification.rs`: general atomic certification authority and RAII permit.
- Delete `crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs`: superseded MVCC-specific name; preserve its behavior in the new module.
- Modify `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`: compile/export certification for every OCC build and route acquisition from the selected concurrency runtime.
- Modify `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs`: own the general authority in every OCC build.
- Modify `crates/wasmtime/src/runtime/transaction/region_runtime.rs`: route shared OCC certification and retain MVCC timestamp validation.
- Modify `crates/wasmtime/src/runtime/transaction/state.rs`: route local/shared certification and use it in direct and benchmark commits.
- Modify `crates/wasmtime/src/runtime/vm/libcalls.rs`: certify and revalidate the real single-version `tfunc` commit path.
- Modify `crates/wasmtime/src/runtime/transaction/tests.rs`: authority, real commit, write-skew, concurrency, and lifecycle regressions.
- Verify, but do not commit as part of the OCC fix, the existing Task 5 draft under `crates/wasmtime/src/runtime/transaction/benchmark/`.

---

### Task 1: General optimistic certification authority

**Files:**
- Create: `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_certification.rs`
- Delete: `crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs:1-60,299-345`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs:1-80,270-335`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs:3770-4145,27430-27535`

**Interfaces:**
- Produces: `OptimisticCertificationAuthority::acquire(self: &Arc<Self>, TransactionId, &BTreeSet<GranuleId>, &BTreeSet<GranuleId>) -> Result<OptimisticCertificationPermit>`
- Produces: `OptimisticCertificationPermit`, which releases every reservation and notifies waiters on `Drop`
- Produces: `CertificationMode::{Shared, Exclusive}`
- Produces: `OptimisticValidation::acquire_commit_certification(...) -> Result<OptimisticCertificationPermit>`
- Produces: `OptimisticValidation::commit_certification_authority() -> Arc<OptimisticCertificationAuthority>`
- Produces: `TransactionConcurrencyRuntime::acquire_optimistic_certification(...) -> Result<OptimisticCertificationPermit>`
- Preserves: compatibility wrappers used by existing MVCC tests until Task 2 routes the shared runtime

- [ ] **Step 1: Add failing single-version authority tests**

In `transaction/tests.rs`, import `OptimisticCertificationAuthority` and add
tests under single-version OCC:

```rust
#[test]
#[cfg(all(
    feature = "transaction-cc-optimistic-validation",
    not(feature = "transaction-mvcc")
))]
fn optimistic_certification_disjoint_permits_coexist() -> Result<()> {
    let authority = Arc::new(OptimisticCertificationAuthority::default());
    let first = mvcc_certification_granule_for_single_version_test(10);
    let second = mvcc_certification_granule_for_single_version_test(11);
    let first_permit = authority.acquire(
        TransactionId::from_raw(1),
        &BTreeSet::from([first]),
        &BTreeSet::from([first]),
    )?;
    let second_permit = authority.acquire(
        TransactionId::from_raw(2),
        &BTreeSet::from([second]),
        &BTreeSet::from([second]),
    )?;
    assert_eq!(authority.lifecycle_counts_for_test()?, (2, 2));
    drop((first_permit, second_permit));
    assert_eq!(authority.lifecycle_counts_for_test()?, (2, 0));
    Ok(())
}
```

Add `optimistic_certification_write_skew_sets_block_atomically` using sets
`T1: reads {a,b}, writes {a}` and `T2: reads {a,b}, writes {b}`. Acquire T1,
start T2 on a named thread, require
`wait_until_blocked_for_test(T2, Duration::from_secs(1)) == true`, drop T1,
join T2, and require its permit uses `a: Shared, b: Exclusive`. Use
`clear_current_thread_transaction_on_drop_for_test()` in the worker closure.

- [ ] **Step 2: Run RED under single-version OCC**

Run:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  optimistic_certification_ --lib -- --nocapture
```

Expected: FAIL because the certification module/types are unavailable without
`transaction-mvcc`.

- [ ] **Step 3: Generalize the authority without changing its algorithm**

Move the complete contents of `mvcc_optimistic.rs` into
`optimistic_certification.rs` and rename only the domain-specific identifiers:

```rust
pub(crate) struct OptimisticCertificationAuthority {
    state: Mutex<CertificationState>,
    changed: Condvar,
}

pub(crate) struct OptimisticCertificationPermit {
    authority: Arc<OptimisticCertificationAuthority>,
    transaction: TransactionId,
    reservations: Vec<(GranuleId, CertificationMode)>,
}
```

Retain `sorted_reservations` exactly: insert all reads as `Shared`, then all
writes as `Exclusive` into a `BTreeMap`. Retain the atomic compatibility check
and all-at-once reservation under one mutex. Change internal error text from
`MVCC certification authority lock is poisoned` to
`optimistic certification authority lock is poisoned`.

In `concurrency/mod.rs`, use these exact feature gates:

```rust
#[cfg(feature = "transaction-cc-optimistic-validation")]
mod optimistic_certification;

#[cfg(all(test, feature = "transaction-cc-optimistic-validation"))]
pub(crate) use self::optimistic_certification::CertificationMode;
#[cfg(feature = "transaction-cc-optimistic-validation")]
pub(crate) use self::optimistic_certification::{
    OptimisticCertificationAuthority,
    OptimisticCertificationPermit,
};
```

Remove the old `mvcc_optimistic` module. If existing MVCC code cannot be
renamed atomically in this task, add crate-private compatibility aliases only:

```rust
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
pub(crate) type MvccCertificationAuthority = OptimisticCertificationAuthority;
#[cfg(all(
    feature = "transaction-mvcc",
    feature = "transaction-cc-optimistic-validation"
))]
pub(crate) type MvccCertificationPermit = OptimisticCertificationPermit;
```

Do not duplicate the authority implementation.

- [ ] **Step 4: Make every OCC policy own the authority**

In `optimistic_validation.rs`, remove the MVCC-only gate from `Arc` and the
certification field:

```rust
#[derive(Debug, Default)]
pub(crate) struct OptimisticValidation {
    pub(crate) read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    pub(crate) write_versions: BTreeMap<(TransactionId, GranuleId), u64>,
    commit_certification: Arc<OptimisticCertificationAuthority>,
}

impl OptimisticValidation {
    pub(crate) fn acquire_commit_certification(
        &self,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<OptimisticCertificationPermit> {
        self.commit_certification.acquire(transaction, reads, writes)
    }

    pub(crate) fn commit_certification_authority(
        &self,
    ) -> Arc<OptimisticCertificationAuthority> {
        self.commit_certification.clone()
    }
}
```

Have existing MVCC-named methods delegate to these methods during the
transition. In `TransactionConcurrencyRuntime`, expose:

```rust
#[cfg(feature = "transaction-cc-optimistic-validation")]
pub(crate) fn acquire_optimistic_certification(
    &self,
    transaction: TransactionId,
    reads: &BTreeSet<GranuleId>,
    writes: &BTreeSet<GranuleId>,
) -> Result<OptimisticCertificationPermit> {
    self.policy
        .acquire_commit_certification(transaction, reads, writes)
}

#[cfg(feature = "transaction-cc-optimistic-validation")]
pub(super) fn optimistic_certification_authority(
    &self,
) -> Arc<OptimisticCertificationAuthority> {
    self.policy.commit_certification_authority()
}
```

- [ ] **Step 5: Run single-version and MVCC authority suites**

Run:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  optimistic_certification_ --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_certification_ --lib -- --nocapture
```

Expected: PASS. Single-version OCC proves two active disjoint permits and
atomic blocking of the write-skew access sets. All existing MVCC authority
ordering, blocking, timestamp, and lifecycle tests remain green.

- [ ] **Step 6: Format only scoped OCC files and commit**

Do not run a formatter over the uncommitted benchmark draft. Format and check
only the Task 1 Rust files:

```bash
rustfmt --edition 2024 \
  crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_certification.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
rustfmt --edition 2024 --check \
  crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_certification.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
```

Then stage only:

```bash
git add \
  crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_certification.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/mvcc_optimistic.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/optimistic_validation.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git diff --cached --check
git commit -m "Generalize optimistic commit certification"
```

Verify `git status --short` still lists the pre-existing Task 5 benchmark draft
and no other new path.

---

### Task 2: Hold OCC certification through every commit path

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs:890-950`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs:820-880,1200-1270,4760-4850`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs:766-930`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs:4150-4425,27430-27535`
- Verify, do not stage yet: `crates/wasmtime/src/runtime/transaction/benchmark/runner.rs`

**Interfaces:**
- Produces: `TransactionRegionRuntime::acquire_optimistic_certification(TransactionId, &BTreeSet<GranuleId>, &BTreeSet<GranuleId>) -> Result<OptimisticCertificationPermit>`
- Produces: `TransactionRegionRuntime::optimistic_certification_counts_for_test() -> Result<(usize, usize)>`
- Produces: `TransactionState::acquire_active_optimistic_certification() -> Result<OptimisticCertificationPermit>` for single-version OCC
- Preserves: `TransactionState::acquire_active_mvcc_certification(...)`, now backed by the generalized authority followed by snapshot validation
- Consumes: the Task 1 general authority in the real `tfunc`, direct state, and benchmark terminal helpers

- [ ] **Step 1: Preserve the benchmark write-skew RED and add real-path RED**

Keep the uncommitted Task 5 test
`transaction::benchmark::runner::tests::write_skew_two_workers_preserves_invariant`
unchanged. Its exact single-version OCC command must still fail with `(0,0)`
before production changes.

Add a production-path regression in `transaction/tests.rs`, gated by
single-version OCC, which instantiates two stores sharing one
`TransactionRegionRuntime` and one VMemory/file-backed transaction storage.
Use the existing transaction WAT operators to export the two roles:

```wat
(tfunc (export "write_skew_a")
  (if (i32.eq (i64.tload (i32.const 64)) (i64.const 1))
    (then (i64.tstore (i32.const 0) (i64.const 0)))))
(tfunc (export "write_skew_b")
  (if (i32.eq (i64.tload (i32.const 0)) (i64.const 1))
    (then (i64.tstore (i32.const 64) (i64.const 0)))))
```

Pause both calls after their reads using the existing transaction test gate,
release together, join without panicking, and require:

```rust
assert!(a + b >= 1, "single-version OCC write skew violated A + B >= 1");
assert_eq!(conflict_count, 1);
```

Name the test `optimistic_validation_single_version_tfunc_prevents_write_skew`.

- [ ] **Step 2: Run both RED regressions**

Run:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  optimistic_validation_single_version_tfunc_prevents_write_skew \
  --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  transaction::benchmark::runner::tests::write_skew_two_workers_preserves_invariant \
  --lib -- --nocapture
```

Expected: FAIL for the real `tfunc` test with the write-skew invariant because
certification is not wired into the production commit entry point. Expected:
FAIL for the benchmark test with the existing `(0,0)` diagnostic.

- [ ] **Step 3: Route the shared and local authority**

In `region_runtime.rs`, add:

```rust
#[cfg(feature = "transaction-cc-optimistic-validation")]
pub(crate) fn acquire_optimistic_certification(
    &self,
    transaction: TransactionId,
    reads: &BTreeSet<GranuleId>,
    writes: &BTreeSet<GranuleId>,
) -> Result<OptimisticCertificationPermit> {
    self.optimistic_certification_authority()?
        .acquire(transaction, reads, writes)
}

#[cfg(feature = "transaction-cc-optimistic-validation")]
fn optimistic_certification_authority(
    &self,
) -> Result<Arc<OptimisticCertificationAuthority>> {
    Ok(self
        .lock_authority()?
        .concurrency
        .optimistic_certification_authority())
}
```

Make `acquire_mvcc_certification` call this generalized acquisition first,
then retain its existing loop requiring every latest committed timestamp to be
at or before the transaction snapshot. Keep MVCC conflict text unchanged.

In `state.rs`, add single-version OCC routing:

```rust
#[cfg(all(
    feature = "transaction-cc-optimistic-validation",
    not(feature = "transaction-mvcc")
))]
pub(crate) fn acquire_active_optimistic_certification(
    &self,
) -> Result<concurrency::OptimisticCertificationPermit> {
    let transaction = self.active_transaction_required()?;
    let reads = self.read_granules.clone();
    let writes = self.write_granules.clone();
    if let Some(region) = &self.shared_region_runtime {
        return region.acquire_optimistic_certification(transaction, &reads, &writes);
    }
    self.concurrency
        .acquire_optimistic_certification(transaction, &reads, &writes)
}
```

Expose test lifecycle counts under single-version OCC and assert zero after
success, conflict, and injected failure paths.

- [ ] **Step 4: Certify the real single-version `tfunc` path before validation**

In `transaction_commit_single_version_impl`:

1. Keep persistent-reference promotion before certification because it may
   finalize staged records/access sets.
2. Begin the shared user-transaction region before waiting for certification,
   matching the MVCC commit/GC ordering.
3. Acquire `_certification` under single-version OCC.
4. Gather the final active read and write granules.
5. Validate all non-object and object reads and all writes against current
   versions while `_certification` is alive.
6. Enter terminal commit and keep the local permit in scope through durable
   LP handling, `complete_commit_with_persistent_root_delta`, committed-error
   cleanup, and object-publication installation.

Use this structure:

```rust
let _user_transaction_region_permit = begin_user_region_if_shared(state)?;

#[cfg(feature = "transaction-cc-optimistic-validation")]
let _certification = store
    .store_opaque_mut()
    .transaction_state_mut()
    .acquire_active_optimistic_certification()?;

let (read_granules, write_granules) = {
    let state = store.store_opaque_mut().transaction_state_mut();
    (state.active_read_granules()?, state.active_write_granules()?)
};
for granule in read_granules {
    let version = current_granule_version(store, instance, granule)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .validate_active_read(granule, version)?;
}
for granule in write_granules {
    let version = current_granule_version(store, instance, granule)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .validate_active_write(granule, version)?;
}
```

Do not skip object granules; `current_granule_version` already routes them to
the object table. Do not drop `_certification` before any terminal return.

- [ ] **Step 5: Certify direct and benchmark state commits**

In `commit_with_read_validation`, acquire a local `_certification` before
`validate_active_read_granules_with` and `validate_active_writes_with`, under
the same single-version OCC feature gate. The permit remains in scope through
`complete_commit()`.

In the non-MVCC branch of `commit_single_tmemory_for_benchmark`, acquire the
permit before capturing/validating versions and retain it through physical
install and `complete_commit`. Do not add a benchmark mutex or change the
write-skew rendezvous.

- [ ] **Step 6: Prove GREEN, disjoint concurrency, and cleanup**

Run the exact regressions again:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  optimistic_validation_single_version_tfunc_prevents_write_skew \
  --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  transaction::benchmark::runner::tests::write_skew_two_workers_preserves_invariant \
  --lib -- --nocapture
```

Expected: PASS; the later write-skew transaction waits for certification,
revalidates, reports an existing optimistic read/write conflict, retries, and
preserves the invariant.

Add and run `optimistic_validation_disjoint_tfunc_commits_overlap_certification`:
two stores use disjoint granules, both permits become active before a test gate
releases either terminal commit, and both commit with zero conflict. Require
`optimistic_certification_counts_for_test()?.1 == 0` afterward.

Add an injected version-mismatch test that acquires certification, fails
validation, aborts, and leaves active permits, snapshots, and terminal commits
at zero.

Run:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  optimistic_validation_ --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  transaction::benchmark::runner --lib -- --nocapture
cargo test -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction::benchmark::runner --lib -- --nocapture
```

- [ ] **Step 7: Run all-policy, unit, and persistence verification**

Run the exact seven-policy forced write-skew loop:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
for OCC_CC in \
  transaction-cc-lockbased \
  transaction-cc-nowait-abort \
  transaction-cc-wound-wait \
  transaction-cc-wait-die \
  transaction-cc-strict-2pl \
  transaction-cc-optimistic-validation \
  transaction-cc-timestamp-ordering
do
  cargo test -p wasmtime --no-default-features \
    --features "$OCC_BASE_FEATURES,$OCC_CC" \
    transaction::benchmark::runner::tests::write_skew_two_workers_preserves_invariant \
    --lib -- --nocapture
done
```

Every policy, including single-version OCC, must pass. Then run:

```bash
export OCC_BASE_FEATURES=anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser
cargo test -q -p wasmtime transaction:: --lib -- --format terse
cargo test -q -p wasmtime --test transaction_persistence -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  transaction:: --lib -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-cc-optimistic-validation" \
  --test transaction_persistence -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-mvcc,transaction-cc-optimistic-validation" \
  transaction:: --lib -- --format terse
cargo test -q -p wasmtime --no-default-features \
  --features "$OCC_BASE_FEATURES,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence -- --format terse
```

Expected: all pass. No test may accept `(0,0)`, and no throughput threshold is
used.

- [ ] **Step 8: Verify scoped state and commit**

Format and check only the OCC correction files. Because the Task 5 draft is
already dirty, inspect both staged and unstaged names explicitly:

```bash
rustfmt --edition 2024 \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
rustfmt --edition 2024 --check \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git diff --check
git status --short
```

Stage only OCC correction files:

```bash
git add \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git diff --cached --check
git commit -m "Certify single-version OCC commits"
```

After committing, `git status --short` must contain only the preserved Task 5
benchmark draft. Record both OCC commits and all exact test totals in the task
report, then return control to benchmark Task 5.
