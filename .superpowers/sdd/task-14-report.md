# Task 14 Report

## Status

Complete. The supported MVCC+OCC build, the default non-MVCC build, and all
seven independently selected single-version concurrency-control builds compile
and pass their transaction suites. Invalid feature combinations fail with the
intended diagnostics. The bounded serializability and commit-preparation
slices complete without deadlock.

Task 14 changed tests only. It did not change production MVCC, concurrency
control, storage, persistence, GC, or recovery code.

## Complete Feature Closure

The minimal feature strings in the original plan no longer provide every
feature required by the current `wasmtime` test module. All `--no-default-
features` test commands therefore used this complete non-MVCC closure:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser
```

Each non-MVCC matrix entry added exactly one `transaction-cc-*` feature. The
supported MVCC entry added:

```text
transaction-mvcc,transaction-cc-optimistic-validation
```

## Verification Results

Formatting and whitespace:

```text
cargo fmt --all
cargo fmt --all -- --check
git diff --check
all exited 0
```

Default non-MVCC:

```text
cargo test -q -p wasmtime transaction:: --lib -- --format terse
585 passed; 0 failed; 2 ignored

cargo test -q -p wasmtime --test transaction_persistence -- --format terse
33 passed; 0 failed
```

Every single-version CC, using the complete closure above:

```text
transaction-cc-lockbased               585 passed; 0 failed; 2 ignored
transaction-cc-nowait-abort            571 passed; 0 failed; 2 ignored
transaction-cc-wound-wait              570 passed; 0 failed; 2 ignored
transaction-cc-wait-die                569 passed; 0 failed; 2 ignored
transaction-cc-strict-2pl              566 passed; 0 failed; 2 ignored
transaction-cc-optimistic-validation   566 passed; 0 failed; 2 ignored
transaction-cc-timestamp-ordering      566 passed; 0 failed; 2 ignored
```

Supported MVCC+OCC:

```text
cargo test -q -p wasmtime --no-default-features \
  --features "$MVCC_MATRIX_FEATURES" transaction:: --lib -- --format terse
694 passed; 0 failed; 2 ignored

cargo test -q -p wasmtime --no-default-features \
  --features "$MVCC_MATRIX_FEATURES" \
  --test transaction_persistence -- --format terse
36 passed; 0 failed
```

Bounded concurrency:

```text
timeout 180 cargo test ... mvcc_serializable_ --lib -- --nocapture
8 passed; 0 failed

timeout 180 cargo test ... mvcc_commit_prepare_ --lib -- --nocapture
11 passed; 0 failed
```

Focused acceptance slices:

```text
cargo test ... mvcc_gc_ --lib
35 passed; 0 failed

cargo test ... mvcc_backend_ --lib
4 passed; 0 failed

cargo test ... mvcc_dax_research_ --lib
1 passed; 0 failed

cargo test ... --test transaction_persistence mvcc_
3 passed; 0 failed
```

Only existing compiler and deprecation warnings were emitted.

## Invalid Feature Diagnostics

Each command exited 101 as required:

```text
runtime,std,gc,transaction-mvcc
error: transaction requires one transaction concurrency-control feature: ...
error: transaction-mvcc currently requires
       transaction-cc-optimistic-validation; ...

runtime,std,gc,transaction-mvcc,transaction-cc-lockbased
error: transaction-mvcc currently requires
       transaction-cc-optimistic-validation; ...

runtime,std,gc,transaction-cc-lockbased,
transaction-cc-optimistic-validation
error: select exactly one transaction concurrency-control feature: ...
```

The no-CC case intentionally emits both applicable diagnostics: the general
transaction requirement and the more specific currently supported MVCC
adapter requirement.

## Verification-Only Adjustments

The first complete MVCC unit run found tests that directly injected or mutated
single-version CC metadata and then expected physical-current-version
validation. MVCC deliberately records snapshot read/write sets without
accessing that metadata and performs serializable OCC certification at
terminal commit. Those tests remain enabled in every non-MVCC build and are
excluded only when `transaction-mvcc` is selected. Existing MVCC snapshot,
certification, write-skew, commit-order, and model-comparison tests cover the
corresponding MVCC behavior.

The permission model remains enabled under MVCC. Its synthetic write
downgrade now removes only the transaction workspace permission in an MVCC
build; it does not demand a nonexistent single-version OCC write-version
record. Both the deterministic downgrade case and generated short schedules
pass.

The strict-2PL matrix initially exposed a pre-existing test assumption: a
publisher cannot acquire a write lock while a suspended observer retains a
read lock. The same focused test failed identically at untouched base
`740f6b150`. The test now asserts the strict-2PL conflict and cleans up both
transactions, while every other non-MVCC CC retains the original stale-read
authority path.

Three persistence tests expected the second parallel writer to fail eagerly.
Under MVCC, both writers correctly stage in parallel and first-writer-wins is
decided at commit. The MVCC branches now assert that the second transaction
commits, the deliberately paused first transaction fails certification after
release with the exact MVCC-certification diagnostic, and restart recovers only
the second transaction's object, tmemory, or mixed-domain value. Non-MVCC
branches retain their original eager-conflict and recovery assertions.

An additional out-of-plan persistence-matrix probe found that the existing
non-MVCC OCC cross-Store conflict fixture does not reject the second writer.
The same focused command fails at the same original assertion on untouched
base `740f6b150`; Task 14 does not encode that pre-existing behavior as an
expected result or alter it. The required non-MVCC matrix remains the complete
transaction unit suite for each independently selected CC plus the default
persistence suite.

## Acceptance Criteria

- Write skew aborts at least one participant:
  `mvcc_serializable_write_skew_aborts_one_writer` stages both participants,
  asserts exactly one commit and one certification conflict, and checks
  `A + B >= 1`.
- Pending and mixed-domain partial commits are never visible:
  `mvcc_serializable_reader_never_mixes_pending_commit_domains` observes the
  old memory/global/table snapshot before and after internal publication, then
  observes all new values from a fresh snapshot.
- Disjoint terminal publications overlap:
  `mvcc_serializable_disjoint_writers_overlap_terminal_publication`
  deterministically waits until both transactions are inside installation and
  then verifies both commits.
- Read-only transactions take no certification latch:
  `mvcc_serializable_read_only_snapshot_stays_stable_during_writer` observes a
  stable `(1, 1)` snapshot and verifies that only the writer increments
  certification and commit lifecycle counts.
- GC cannot enter with active snapshots or prepared commits:
  the `mvcc_gc_` slice checks snapshot and pending-commit rejection for the
  coordinator barrier and for every pluggable GC entry point.
- Current-state GC sees no historical versions:
  the GC policy/rebase tests verify that callbacks receive only validated
  current `ObjectTable` slots and that historical sidecars are absent from
  collector reachability.
- History disappears on rebase and restart:
  full-rebase tests clear all six sidecar tables and advance the baseline;
  file-backed reopen tests recover value 22 as timestamp-zero current state
  with no MVCC histories or version records.
- Backends remain runtime-selectable:
  the same MVCC fixture passes for VMemory, file-backed memory, and research
  DAX PMEM; cloned or reopened runtimes begin with no sidecar history.
- Non-MVCC behavior remains intact:
  the default suites and all seven single-version CC suites pass, including
  the corrected strict-2PL lock expectation.

## Constrained Edit-Surface Audit

`git diff --name-only ca6090dca` contains feature manifests, transaction
runtime modules and tests, transaction libcalls, tmemory implementation files,
the design/implementation documents, and local SDD reports. It contains no
parser, validator, Cranelift implementation, or unrelated Wasmtime subsystem
change.

The backend/recovery paths were investigated explicitly:

- `transaction/persist.rs` only initializes the previously added
  `committed_tmemory_pages` field in two recovery test fixtures.
- `tmemory/block_region.rs` reports the largest committed size from existing
  size publications; it does not add or alter an on-disk record.
- `tmemory.rs` contains the reviewed Task 10 capacity-tail support used to
  prepare, install, and roll back growth beyond the currently visible size
  across the existing runtime-selected backends.

There is no durable MVCC version-chain encoding and no recovery of historical
versions. Task 14 itself changes only:

```text
crates/wasmtime/src/runtime/transaction/tests.rs
crates/wasmtime/tests/transaction_persistence.rs
.superpowers/sdd/task-14-report.md
```

## Human Gate

Independent review found no Critical or Important issues. Its two substantive
checks hardened the persistence fixtures to require the exact MVCC
certification diagnostic and kept the non-MVCC conflict oracle limited to its
legacy read/write diagnostics. The final review result was Ready.

No upstream action was taken. Under the Bytecode Alliance AI Tool Use Policy,
opening, reviewing, or commenting on a Wasmtime pull request remains a
human-only action.
