# MVCC Concurrency-Control Adapters Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow the optional `transaction-mvcc` compile-time visibility feature to pair with each existing exactly-one concurrency-control feature while preserving serializability, non-MVCC behavior, durability, backend independence, and pluggable GC.

**Architecture:** Keep `SelectedTransactionVisibility` and `SelectedConcurrencyControl` as independent compile-time axes. MVCC reads remain lock-free, first writes enter a selected-policy writer-admission adapter, and every MVCC writer acquires one common ordered read/write certification permit before snapshot-timestamp validation and retains it through terminal publication and cleanup. Generalize the existing MVCC+OCC terminal pipeline rather than duplicating durability, recovery, or version-sidecar code for each CC.

**Tech Stack:** Rust, Cargo feature gates, Wasmtime transaction runtime, typed MVCC sidecars, the existing durable transaction log, and Python benchmark/report tools.

## Global Constraints

- `transaction-mvcc` remains an optional compile-time feature.
- Every `transaction` build selects exactly one `transaction-cc-*` feature.
- All seven CC policies compile both with and without MVCC, producing fourteen primary configurations.
- MVCC snapshot reads never acquire current-state read locks or current-version read metadata.
- The selected CC controls writer admission; common MVCC certification is the final serializability boundary.
- Every MVCC pairing prevents lost updates and write skew.
- No durable record, recovery decision, Wasm syntax, validator, or compiler-lowering changes.
- Backend and persistent-GC policy selection remain independent from visibility and CC.
- Historical object versions remain sidecar data and never become object-table entries or GC roots.
- Non-MVCC paths and policy semantics remain unchanged.
- Follow the Bytecode Alliance AI Tool Use Policy. Do not open, review, or comment on Wasmtime pull requests or issues.
- Do not add an agent, model, or tool to commit authorship annotations.

---

## File Structure

- `crates/wasmtime/src/runtime/transaction/config.rs`: compile-axis validation.
- `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`: common certification ownership and selected writer adapter.
- `crates/wasmtime/src/runtime/transaction/concurrency/*.rs`: minimal policy-specific writer overrides and tests.
- `crates/wasmtime/src/runtime/transaction/region_runtime.rs`: shared/local authority routing.
- `crates/wasmtime/src/runtime/transaction/state.rs`: MVCC workspace acquisition, commit, benchmark, and cleanup.
- `crates/wasmtime/src/runtime/vm/libcalls.rs`: real mixed-domain terminal commit.
- `crates/wasmtime/src/runtime/store.rs`: suspended/terminal cleanup gates.
- `crates/wasmtime/src/runtime/transaction/tests.rs`: cross-policy semantics and lifecycle regressions.
- `crates/wasmtime/tests/all/transaction_persistence.rs`: durable pairing coverage.
- `crates/wasmtime/src/runtime/transaction/benchmark/*.rs`: benchmark identity and generalized commit driver.
- `scripts/transaction-cc-bench.py`, `scripts/transaction_cc_bench_report.py`, and their tests: fourteen-build orchestration and reporting.
- `docs/shisoft/2026-07-31-transaction-cc-benchmark.md`: updated operator guide.

---

### Task 1: Make MVCC and CC Independent Compile-Time Axes

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

**Interfaces:**
- Consumes: existing exactly-one CC checks and `OptimisticCertificationAuthority`.
- Produces: `MvccCertificationAuthority` and `MvccCertificationPermit` for every MVCC build, including strict 2PL.
- Preserves: existing non-MVCC certification gates and behavior.

- [ ] **Step 1: Verify the unsupported-pair RED**

Run:

```bash
cargo test -p wasmtime --features transaction-mvcc \
  mvcc_is_visibility_not_concurrency_control --lib -- --exact
```

Expected: compilation fails with the current “MVCC requires optimistic validation” diagnostic.

- [ ] **Step 2: Generalize one certification lifecycle test**

Change an existing MVCC certification lifecycle test to `#[cfg(feature = "transaction-mvcc")]`. It must acquire one shared-runtime permit, assert lifecycle counts `(1, 1)`, drop it, and assert `(1, 0)`.

- [ ] **Step 3: Confirm strict-2PL RED**

```bash
cargo test -p wasmtime --no-default-features \
  --features 'std,runtime,cranelift,wat,transaction-mvcc,transaction-cc-strict-2pl' \
  mvcc_certification --lib -- --nocapture
```

Expected: compilation fails because common certification is currently excluded by strict-2PL/OCC-specific gates.

- [ ] **Step 4: Generalize certification compilation**

Use these gates in `concurrency/mod.rs`:

```rust
#[cfg(any(not(feature = "transaction-cc-strict-2pl"), feature = "transaction-mvcc"))]
mod optimistic_certification;

#[cfg(any(not(feature = "transaction-cc-strict-2pl"), feature = "transaction-mvcc"))]
use self::optimistic_certification::{
    OptimisticCertificationAuthority,
    OptimisticCertificationPermit,
};

#[cfg(feature = "transaction-mvcc")]
pub(crate) type MvccCertificationAuthority = OptimisticCertificationAuthority;

#[cfg(feature = "transaction-mvcc")]
pub(crate) type MvccCertificationPermit = OptimisticCertificationPermit;
```

Compile `ConcurrencyControlState::commit_certification` under the same `any` condition. Gate MVCC-named acquisition and test accessors only on `transaction-mvcc`; preserve the existing non-MVCC APIs.

- [ ] **Step 5: Remove only the MVCC+OCC restriction**

Delete the compile-error block requiring optimistic validation from `config.rs`. Leave zero-CC and multiple-CC diagnostics unchanged. Generalize MVCC terminal imports/types that still carry an unnecessary optimistic feature gate.

- [ ] **Step 6: Verify positive and negative selection**

Run the lifecycle test under MVCC+lockbased, MVCC+strict-2PL, and MVCC+optimistic. Then require no-CC and two-CC MVCC checks to fail with the existing exactly-one diagnostics.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/config.rs \
  crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Allow MVCC with every concurrency policy"
```

---

### Task 2: Route First MVCC Writes Through the Selected Policy

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/concurrency/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

**Interfaces:**
- Consumes: `TransactionConcurrencyControl::acquire_granule_write`, conflict actions, and workspace access sets.
- Produces: `acquire_mvcc_granule_write` routing through local/shared selected-policy authority.
- Preserves: MVCC reads remain workspace-only and non-MVCC acquisition is untouched.

- [ ] **Step 1: Add failing adapter tests**

Add feature-neutral MVCC tests:

```rust
#[test]
fn mvcc_snapshot_read_does_not_enter_selected_cc() {
    let runtime = TransactionRegionRuntime::default();
    let mut state = TransactionState::default();
    let transaction = state.begin_with_region_runtime(&runtime).unwrap();
    let granule = global_granule_id(None, 0);

    assert!(state.acquire_granule_read(granule, 0).unwrap());
    assert!(state.owns_granule_read(granule));
    assert_eq!(
        runtime
            .selected_policy_read_granules_for_test(transaction)
            .unwrap(),
        Vec::<GranuleId>::new()
    );
    assert_eq!(
        runtime
            .selected_policy_write_granules_for_test(transaction)
            .unwrap(),
        Vec::<GranuleId>::new()
    );
    state.abort().unwrap();
}

#[test]
fn mvcc_first_write_enters_selected_cc_once() {
    let runtime = TransactionRegionRuntime::default();
    let mut state = TransactionState::default();
    let transaction = state.begin_with_region_runtime(&runtime).unwrap();
    let granule = global_granule_id(None, 0);

    assert!(state.acquire_granule_write(granule, 0).unwrap());
    assert!(!state.acquire_granule_write(granule, 0).unwrap());
    assert_eq!(
        runtime
            .selected_policy_write_granules_for_test(transaction)
            .unwrap(),
        vec![granule]
    );
    state.abort().unwrap();
}
```

Add test-only selected-policy read/write set accessors on `ConcurrencyControlState` and `TransactionRegionRuntime`. Inspect real policy state.

- [ ] **Step 2: Verify RED under MVCC+lockbased**

Run both exact tests. The writer test must fail because MVCC currently bypasses the CC authority.

- [ ] **Step 3: Add policy-neutral writer admission**

Add:

```rust
#[cfg(feature = "transaction-mvcc")]
pub(crate) fn acquire_mvcc_granule_write(
    &mut self,
    transaction: TransactionId,
    granule: GranuleId,
    current_version: u64,
) -> Result<TransactionConflictAction> {
    self.policy
        .acquire_granule_write(transaction, granule, current_version)
}
```

Expose it through `TransactionRegionRuntime` with the existing authority lock and conflict-aborted bookkeeping.

- [ ] **Step 4: Route only first writes**

In the MVCC branch of `TransactionState::acquire_granule_write`, return `false` immediately for a repeated write, otherwise call the adapter and `handle_conflict_action` before inserting workspace sets. Insert writes into both read and write sets because partial writes consume snapshot state. Keep `acquire_granule_read` lock-free.

- [ ] **Step 5: Add deterministic collision checks**

Use real shared-runtime states to assert:

- no-wait and strict 2PL reject a colliding second writer;
- wait-die and wound-wait preserve their existing age ordering;
- lockbased preserves its existing owner displacement rule;
- optimistic and timestamp configurations may defer the final snapshot conflict to commit.

Use barriers/test hooks, not sleeps, and assert no leaked owner after abort.

- [ ] **Step 6: Run across seven MVCC pairings**

```bash
for cc in lockbased nowait-abort optimistic-validation strict-2pl timestamp-ordering wait-die wound-wait; do
  cargo test -p wasmtime --no-default-features \
    --features "std,runtime,cranelift,wat,transaction-mvcc,transaction-cc-$cc" \
    mvcc_ --lib -- --format terse || exit 1
done
```

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/concurrency/mod.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Adapt MVCC writer admission to selected CC"
```

---

### Task 3: Generalize Serializable MVCC Commit

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/store.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/storage.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`

**Interfaces:**
- Consumes: selected writer ownership, complete workspace sets, snapshot timestamp, and common certification.
- Produces: one MVCC writer terminal pipeline for all selected CCs.
- Preserves: exact pre-LP rollback and post-LP force-completion ordering.

- [ ] **Step 1: Generalize the write-skew test gate**

Gate `mvcc_serializable_write_skew_aborts_one_writer` only on MVCC. Keep:

```rust
assert_eq!(committed, 1);
assert_eq!(conflicted, 1);
assert!(left + right >= 1, "MVCC write skew violated A + B >= 1");
```

Accept a selected-policy conflict or MVCC certification conflict, but reject every unexpected error.

- [ ] **Step 2: Verify RED under MVCC+no-wait**

Run the exact write-skew test. Expected: compile/execution failure because terminal functions remain OCC-gated.

- [ ] **Step 3: Generalize MVCC-only gates deliberately**

In `state.rs`, `region_runtime.rs`, `libcalls.rs`, and `store.rs`, replace the combined MVCC+optimistic gates only for types/functions that consume MVCC state or participate in MVCC terminal commit. Do not alter unrelated CC-specific code.

- [ ] **Step 4: Validate selected writer state**

Before common certification, validate every write granule through the selected policy. Do not validate historical reads through current-version CC read APIs. Obtain current versions through the same domain-specific sources used by existing single-version write validation.

- [ ] **Step 5: Retain exact terminal order**

```text
selected writer validation
-> ordered MVCC certification
-> latest timestamp <= snapshot validation
-> COW/durable prepare
-> durable commit LP
-> current-state installation
-> atomic MVCC visibility publication
-> selected-policy commit result
-> snapshot unregister
-> selected-policy release
-> certification release
```

Read-only transactions skip writer validation/certification but still finish policy lifecycle and unregister once.

- [ ] **Step 6: Generalize benchmark MVCC commit**

Gate the benchmark MVCC terminal state, version counters, failure recovery, and `commit_single_tmemory_for_benchmark_mvcc` only on MVCC. Count versions for every MVCC pairing.

- [ ] **Step 7: Run serializability regressions**

For all seven MVCC features, run the forced write-skew, lost-update, disjoint-commit, and read-only snapshot tests. Require no hang, unexpected error, active snapshot, policy owner, or certification permit.

- [ ] **Step 8: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/store.rs \
  crates/wasmtime/src/runtime/transaction/benchmark/storage.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs
git commit -m "Generalize serializable MVCC commit policy"
```

---

### Task 4: Harden Cleanup, Durability, and Policy Semantics

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/state.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/region_runtime.rs`
- Modify: `crates/wasmtime/src/runtime/vm/libcalls.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs`
- Modify: `crates/wasmtime/tests/all/transaction_persistence.rs`
- Modify only when necessary: the seven files in `transaction/concurrency/`

**Interfaces:**
- Consumes: common terminal state plus selected-policy reservations.
- Produces: exact-once cleanup for every policy and durable failure boundary.
- Preserves: error composition, storage incarnation rules, and recovery compatibility.

- [ ] **Step 1: Add failing dual-resource cleanup tests**

After injected certification, preparation, pre-LP, and post-LP failures, assert:

```rust
assert_eq!(runtime.mvcc_certification_counts_for_test()?, (1, 0));
assert_eq!(
    runtime
        .visibility_for_test()
        .runtime()
        .snapshot_lifecycle_counts_for_test()?
        .0,
    0
);
assert!(
    runtime
        .selected_policy_read_granules_for_test(transaction)?
        .is_empty()
);
assert!(
    runtime
        .selected_policy_write_granules_for_test(transaction)?
        .is_empty()
);
assert!(!state.has_mvcc_terminal_commit());
```

Post-LP tests instead require committed current/MVCC state and no leaked resources after force completion.

- [ ] **Step 2: Verify RED under strict 2PL and wound-wait**

Run the generalized failure suite under both pairings. Expected failure must identify missing selected-policy cleanup or a wrong terminal transition, never a timeout.

- [ ] **Step 3: Unify exact-once cleanup**

Make commit/abort cleanup perform visibility completion, selected-policy commit result when committed, selected-policy release, certification release, and conflict/terminal bookkeeping cleanup exactly once. Use `combine_operation_and_cleanup_results`. Retain retryable terminal state after post-LP failure.

- [ ] **Step 4: Add policy-specific real schedules**

Prove each adapter is meaningful:

- lockbased: deterministic owner displacement;
- no-wait: immediate writer abort;
- strict 2PL: write lock retained through terminal completion while snapshot readers remain absent from its reader locks;
- wait-die and wound-wait: deterministic age behavior;
- timestamp ordering: existing writer timestamp rule remains active;
- optimistic: execution remains private until certification.

Add overrides to an individual policy only if its existing single-version write call implicitly applies a current-read rule invalid for MVCC. Keep each override in its policy file.

- [ ] **Step 5: Generalize durable tests**

For every MVCC pairing verify file-backed pre-LP rollback, post-LP exact-once completion, recovery to timestamp-zero baseline, exact storage incarnation cleanup, and cross-policy durable-image compatibility. Run existing research-DAX coverage without changing durable formats.

- [ ] **Step 6: Verify GC independence**

Under MVCC+strict-2PL and MVCC+timestamp, run both current-state-only and MVCC-compliant GC policy tests. Require identical restricted views and zero prunable versions after final barrier. Run a non-MVCC GC build to ensure MVCC barriers are absent.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/state.rs \
  crates/wasmtime/src/runtime/transaction/region_runtime.rs \
  crates/wasmtime/src/runtime/vm/libcalls.rs \
  crates/wasmtime/src/runtime/transaction/tests.rs \
  crates/wasmtime/src/runtime/transaction/concurrency \
  crates/wasmtime/tests/all/transaction_persistence.rs
git commit -m "Harden MVCC adapter terminal cleanup"
```

---

### Task 5: Expand Benchmark and Reports to Fourteen Builds

**Files:**
- Modify: `scripts/transaction-cc-bench.py`
- Modify: `scripts/transaction_cc_bench_report.py`
- Modify: `scripts/tests/test_transaction_cc_bench.py`
- Modify: `scripts/tests/test_transaction_cc_bench_report.py`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/config.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/record.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/mod.rs`
- Modify: `crates/wasmtime/src/runtime/transaction/benchmark/tfunc.rs`
- Modify: `docs/shisoft/2026-07-31-transaction-cc-benchmark.md`

**Interfaces:**
- Consumes: fourteen valid feature combinations and existing workloads.
- Produces: explicit visibility and CC dimensions plus paired MVCC/single-version comparisons.

- [ ] **Step 1: Add Python RED for fourteen policies**

Define seven base names/features in the test and require both each base name and `mvcc-{base}`. Each entry has exactly one CC feature; only MVCC names contain `transaction-mvcc`.

- [ ] **Step 2: Run RED**

```bash
python3 -m unittest scripts.tests.test_transaction_cc_bench -v
```

Expected: failure because only `mvcc-optimistic` exists.

- [ ] **Step 3: Generate the expanded map**

```python
BASE_POLICIES = {
    "lockbased": "transaction-cc-lockbased",
    "no-wait": "transaction-cc-nowait-abort",
    "optimistic": "transaction-cc-optimistic-validation",
    "strict-2pl": "transaction-cc-strict-2pl",
    "timestamp": "transaction-cc-timestamp-ordering",
    "wait-die": "transaction-cc-wait-die",
    "wound-wait": "transaction-cc-wound-wait",
}
POLICIES = {
    **{name: [feature] for name, feature in BASE_POLICIES.items()},
    **{
        f"mvcc-{name}": ["transaction-mvcc", feature]
        for name, feature in BASE_POLICIES.items()
    },
}
```

Validate MVCC membership by prefix rather than one hard-coded name.

- [ ] **Step 4: Separate identity in Rust records**

Add `compiled_visibility_mode() -> "single-version" | "mvcc"` and `compiled_concurrency_control_name()`. Compose the unique policy name from those values. Store `visibility_mode` and `concurrency_control` in driver, cell, skip, and tfunc records and CSV rows. Increment `SCHEMA_VERSION` consistently because the machine-readable contract changes.

- [ ] **Step 5: Add reporter RED**

Require rejection of inconsistent policy/visibility/CC/features and missing matching base/MVCC pairs. Require a Markdown “MVCC versus matching single-version CC” section containing one ratio per backend/workload/worker/repetition/seed/base-CC key.

- [ ] **Step 6: Generalize comparisons**

Index baselines by:

```python
(
    record["concurrency_control"],
    record["backend"],
    record["workload"],
    record["workers"],
    record["repetition"],
    record["seed"],
)
```

Compare every MVCC row to its matching single-version row. Preserve scaling, backend, abort/retry, GC/version, and tfunc sections.

- [ ] **Step 7: Update guide and tests**

Document fourteen builds and the independent axes. Run:

```bash
python3 -m unittest \
  scripts.tests.test_transaction_cc_bench \
  scripts.tests.test_transaction_cc_bench_report -v
python3 -m py_compile \
  scripts/transaction-cc-bench.py \
  scripts/transaction_cc_bench_report.py
```

Then run one VMemory read-only/write-skew smoke request for every MVCC policy and validate the combined report.

- [ ] **Step 8: Commit**

```bash
git add scripts/transaction-cc-bench.py \
  scripts/transaction_cc_bench_report.py \
  scripts/tests/test_transaction_cc_bench.py \
  scripts/tests/test_transaction_cc_bench_report.py \
  crates/wasmtime/src/runtime/transaction/benchmark \
  docs/shisoft/2026-07-31-transaction-cc-benchmark.md
git commit -m "Benchmark all MVCC concurrency adapters"
```

---

### Task 6: Full Matrix, Remote Benchmark, and Audit

**Files:**
- Modify only files required by evidence-backed failures.
- Verify all changes since design commit `ba6603758`.

**Interfaces:**
- Consumes: complete implementation.
- Produces: clean verified commits and reproducible benchmark artifacts.
- Preserves: no push, PR, issue interaction, or AI/tool authorship annotation.

- [ ] **Step 1: Format and inspect**

```bash
cargo fmt --all -- --check
git diff --check
git status --short
git diff ba6603758..HEAD --stat
```

Apply formatting only if required and inspect all formatter changes.

- [ ] **Step 2: Run fourteen library suites**

```bash
for cc in lockbased nowait-abort optimistic-validation strict-2pl timestamp-ordering wait-die wound-wait; do
  for visibility in single mvcc; do
    features="anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,debug-builtins,runtime,component-model,component-model-async,threads,stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-cc-$cc"
    test "$visibility" = mvcc && features="$features,transaction-mvcc"
    cargo test -p wasmtime --no-default-features --features "$features" --lib || exit 1
  done
done
```

Expected: fourteen successful suites.

- [ ] **Step 3: Run fourteen persistence suites**

Use the same loop with:

```bash
cargo test -p wasmtime --no-default-features --features "$features" \
  --test transaction_persistence -- --nocapture
```

Expected: fourteen successful suites.

- [ ] **Step 4: Verify MVCC-free compilation and Python tools**

```bash
cargo check -p wasmtime --no-default-features \
  --features 'std,runtime,cranelift,wat'
python3 -m unittest discover -s scripts/tests -p 'test_transaction_cc_bench*.py' -v
python3 -m py_compile scripts/transaction-cc-bench.py scripts/transaction_cc_bench_report.py
```

- [ ] **Step 5: Run comprehensive benchmark on `.87`**

Bundle exact clean `HEAD`, copy it to `shisoft@192.168.10.87`, check out that revision in a fresh `/tmp` checkout, and run:

```bash
python3 -u scripts/transaction-cc-bench.py \
  --profile quick \
  --output-dir target/transaction-bench/<git-short-revision>
```

Require all fourteen policy builds to complete. Regenerate the report, copy artifacts under local ignored `target/transaction-bench/`, and checksum `invocation.json`, `orchestrator.jsonl`, `results.jsonl`, `results.csv`, and `summary.md`.

- [ ] **Step 6: Fix evidence-backed failures systematically**

For each compiler error, invariant violation, lifecycle leak, or report rejection: reproduce the smallest pairing, identify the first incorrect transition, retain a regression test, implement the narrow fix, rerun that pairing, and rerun the full matrix whenever shared code changed.

- [ ] **Step 7: Final audit**

```bash
git status --short
git log --format='%H %s%n%an <%ae>%n%cn <%ce>%n%b' ba6603758..HEAD
git diff --check ba6603758..HEAD
```

Require a clean worktree, human author/committer metadata, no forbidden co-author annotation, and no upstream push.

- [ ] **Step 8: Report**

Report the commit range, fourteen library/persistence results, zero write-skew/lost-update anomalies, policy-specific writer behavior, GC version accounting, paired performance on both backends, artifact links, and the short-sample limitation.
