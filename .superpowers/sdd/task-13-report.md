# Task 13 Report

## Status

Complete. Task 13 adds test-only verification that MVCC history remains a
volatile runtime sidecar across VMemory, file-backed memory, and research-mode
DAX PMEM. File-backed restart recovers only the latest durable current state as
a fresh timestamp-zero baseline. No production, backend, durable-format,
publication, GC, or recovery implementation was changed.

## Coverage

The unit backend fixture uses the same Wasm `i32.tload`/`i32.tstore` and
`tfunc` paths for all three runtime-selected backends. It proves:

- the instantiated `TMemory` reports the configured backend and the
  configuration reports its exact file/DAX backing and persistence mode;
- commits of values 11 and 22 create a two-version memory chain while a live
  predecessor snapshot remains registered;
- a current transaction and the physical current-state API read 22 while the
  predecessor snapshot continues to read 11;
- file-backed memory uses its existing ownerless physical key while VMemory
  and DAX retain instance-owned keys;
- cloning the backend configuration into a fresh runtime carries no history;
- fresh runtimes have visible and baseline timestamp zero, no oldest active
  snapshot, no snapshot/commit/certification registrations, and zero version
  chains.

The file-backed reopen unit test first proves that the old runtime owns
sidecar chains. It then drops that runtime, reopens through the existing
recovery path, and proves zero MVCC metadata and chains before reading the
latest durable value 22 as the unversioned timestamp-zero baseline.

The integration restart test uses bounded synchronous channels with five-second
timeouts. A live old transaction reads value 11, pauses, and reads 11 again
after another Store commits value 22. A current transaction reads 22. After all
old Stores and the old runtime are dropped, recovery exposes no current-data
winner, object winner, root, or loose undo that could represent MVCC history,
and a reopened Store reads the physical durable value 22.

The existing Task 10 growth-tail tests remain the authoritative fault-boundary
fixtures. Their assertions now explicitly cover latest-only recovery metadata:

- failure before LP exposes the prior zero tail and one existing loose undo
  rollback, with only the committed size winner;
- successful publication through and after LP exposes the committed tail,
  no loose undo rollback, and only the committed size winner.

## TDD Evidence

The first VMemory test was added before its reusable fixture. The focused RED
failed only on that missing test fixture:

```text
error[E0425]: cannot find function `mvcc_backend_assert_snapshot_history`
  --> crates/wasmtime/src/runtime/transaction/tests.rs:844:5

cargo test ... mvcc_backend_vmemory_keeps_history_above_current_state_storage --lib
exit 101
```

After implementing the fixture, the first focused GREEN was:

```text
mvcc_backend_vmemory_keeps_history_above_current_state_storage
1 passed; 0 failed
```

The first three-backend matrix run exposed one fixture assumption rather than
a product defect: the file-backed chain key is deliberately ownerless because
Stores share one physical tmemory. VMemory and research DAX passed on that run.
Selecting `None` for the existing file-backed owner identity made the complete
matrix GREEN:

```text
mvcc_backend_ filter
4 passed; 0 failed

mvcc_dax_research_ filter
1 passed; 0 failed

transaction_persistence mvcc_ filter
3 passed; 0 failed
```

The integration restart test passed on its first focused run, confirming that
the requested latest-only recovery behavior already existed and required no
production change.

## Verification

All MVCC commands used the complete feature closure:

```text
anyhow,async,backtrace,cache,gc,gc-copying,gc-drc,gc-null,wat,profiling,
parallel-compilation,cranelift,pooling-allocator,demangle,addr2line,coredump,
debug-builtins,runtime,component-model,component-model-async,threads,
stack-switching,std,debug,compile-time-builtins,wit-parser,transaction-mvcc,
transaction-cc-optimistic-validation
```

Fresh results:

```text
cargo test ... mvcc_backend_ --lib
4 passed; 0 failed

cargo test ... mvcc_dax_research_ --lib
1 passed; 0 failed

cargo test ... --test transaction_persistence mvcc_
3 passed; 0 failed

timeout 180 cargo test ... mvcc_serializable_ --lib
8 passed; 0 failed

timeout 180 cargo test ... mvcc_commit_ --lib
22 passed; 0 failed

cargo test ... mvcc_gc_ --lib
35 passed; 0 failed

cargo test ... runtime::transaction::mvcc:: --lib
33 passed; 0 failed

cargo test ... persistent_gc --lib
48 passed; 0 failed

cargo check ... (complete MVCC feature closure)
exit 0

cargo check -p wasmtime
exit 0

cargo check ... (complete non-MVCC OCC feature closure)
exit 0

cargo fmt --all -- --check
exit 0

git diff --check
exit 0
```

Only pre-existing workspace warnings were emitted.

## Durable-Format Audit

The required audit was run:

```text
git diff ca6090dca -- \
  crates/wasmtime/src/runtime/transaction/persist.rs \
  crates/wasmtime/src/runtime/vm/block_region \
  crates/wasmtime/src/runtime/vm/memory/tmemory
```

It contains 49 diff lines and six pre-existing Task 10 insertions:

- two test initializers for `committed_tmemory_pages: None`;
- the `committed_tmemory_pages` recovery-report field;
- collection and return of that existing field.

Relative to Task 13's base commit, the audited production paths have zero
changed lines. There is no durable version-chain encoding, backend-owned
history, recovery record, or recovery decision change.

## Deviations

The corrected Task 13 brief supersedes the older plan's research-DAX restart
step. `TMemoryDaxPmemBacking::ResearchTemp` is tested only within one runtime
and against a newly constructed empty runtime; no claim is made that its bytes
survive destruction. Hardware-required fsdax reopen tests remain unrun because
the host was not explicitly configured for PMEM.
