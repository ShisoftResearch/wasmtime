# Task 10 Report

## Status

Complete. MVCC commit publication now has an explicit terminal state machine:
failures before the commit LP abort and restore typed current-state
predecessors, while failures after the durable LP or markerless irreversible
decision force-complete every final value before returning an error.

## Commit

`Handle MVCC publication failures around commit LP`

No co-author annotation was added.

## Implementation

- Retains one `MvccTerminalCommitState` containing the pending record,
  certification permit, user-region/GC permit, prepared values, typed
  installation progress, durable publication progress, root/version progress,
  and the original completion error.
- Records a typed key only after its physical install succeeds. Pre-LP rollback
  restores installed objects, tables, globals, and memories in reverse order,
  then removes the aborted record's heads by record identity.
- Keeps partial rollback progress and the certification/snapshot/GC exclusion
  alive when rollback itself fails. Both the compiled `tfunc` entry boundary
  and host-selected commit path drive an explicit retry before new staging.
- Makes batch preparation all-or-nothing: a late typed-domain preparation error
  removes every earlier head appended for that same record while the domain
  metadata lock is still held.
- Treats a durable commit LP, and the corresponding markerless irreversible
  boundary, as a one-way decision. Force completion idempotently installs
  sizes, remaining memory/global/table values, volatile and mapped objects,
  persistent roots, version bumps, the shared commit record, and cleanup.
- Keeps a reversible markerless commit abortable through the fallible atomic
  shared-record publication. Successful publication immediately changes the
  terminal decision to force-commit before any post-transition hook or other
  fallible cleanup, so a committed record can never be rolled back.
- Separates persistent-root progress from object-version progress and
  prevalidates version updates before applying them, so retries cannot
  duplicate a partial update.
- Avoids reinstalling a terminal state after cleanup has already cleared the
  active workspace. Returned errors distinguish “committed durably” from
  markerless “committed irrevocably.”

## Growth-tail durability exception

The user approved a narrow backend/recovery exception for commit-time MVCC
growth. File-backed and DAX/VMemory storage expose capacity-bounded internal
operations so MVCC can:

1. reserve the new backing capacity without publishing the logical size;
2. publish the existing tmemory undo record;
3. install the hidden tail;
4. publish the logical-size record and commit LP.

Creation and publication entry points are gated by `transaction-mvcc`.
Ordinary reads, writes, and granule metadata remain bounded by committed
logical length, so non-MVCC publication behavior is unchanged. The underlying
capacity-bounded restore operation remains available without MVCC so a
non-MVCC recovery build can apply an existing loose undo written into a hidden
tail. No durable record format or recovery decision rule changed.

## Fault coverage

The deterministic `mvcc_commit_fault_` tests cover:

- after certification;
- after preparation;
- after first memory, global, and table install;
- after durable publication and the existing immediately-before-LP hook;
- immediately after LP;
- after memory-size and table-size install;
- before and after mapped-object installation;
- during persistent-root apply;
- immediately before shared-record publication;
- after commit-record transition;
- during cleanup;
- during a partially completed rollback;
- compiled `tfunc` and host-selected retry boundaries.

Pre-LP tests assert an aborted record, removed failed heads/object chains,
physical predecessors, empty persistent roots, no live promoted objects,
released certification and snapshot registrations, GC-barrier admission, and
fresh predecessor reads. Post-LP tests assert a committed shared record, all
typed physical finals, mapped/root object state, no retained terminal or
permits, and fresh all-domain final reads.

The markerless pre-record-publication regression uses a same-size VMemory
memory write. Scalar-global and funcref-table writes currently produce empty
persistent-root publications, so including either would create a durable marker
and LP rather than exercise the markerless path. Their reversible physical and
sidecar rollback remains covered by the mixed-domain pre-LP cut-point test.

## Persistence and compatibility coverage

- A real file-backed `tfunc` failure before LP grows and writes a tail, then
  proves the later logical grow exposes zeros and recovery applies the loose
  undo.
- A real committed grow survives restart with its logical size and tail bytes.
- Low-level committed and loose growth-tail recovery tests validate LP
  classification and undo selection.
- Ordinary logical APIs reject reserved-capacity addresses in both MVCC and
  non-MVCC builds.
- An unconditional non-MVCC test constructs a hidden file-backed tail through
  private capacity primitives, publishes a loose ordinary undo, recovers at
  logical size one, grows to size two, and observes the restored zero tail.

## Verification

Fresh final verification used the repository's complete unit-test feature
closure because the plan's minimal `cargo test --lib` closure does not compile
unrelated unconditional Wasmtime tests.

```text
rustfmt --check --edition 2024 <all changed Rust files>
exit 0

git diff --check
exit 0

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_commit_ --lib -- --format terse
22 passed; 0 failed

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-mvcc,transaction-cc-optimistic-validation" \
  --test transaction_persistence mvcc_file_backed_lp_ -- --format terse
2 passed; 0 failed

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-mvcc,transaction-cc-optimistic-validation" \
  mvcc_file_backed_lp_ --lib -- --format terse
2 passed; 0 failed

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-mvcc,transaction-cc-optimistic-validation" \
  ordinary_logical_bounds_reject_reserved_capacity_addresses --lib
1 passed; 0 failed

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-cc-optimistic-validation" \
  ordinary_logical_bounds_reject_reserved_capacity_addresses --lib
1 passed; 0 failed

cargo test -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-cc-optimistic-validation" \
  file_backed_recovery_restores_hidden_growth_tail_without_mvcc_feature --lib
1 passed; 0 failed

cargo check -p wasmtime --no-default-features \
  --features "<complete closure>,transaction-cc-optimistic-validation"
exit 0

cargo check -p wasmtime
exit 0
```

Only existing workspace warnings were emitted.

## Scope

MVCC remains a visibility feature paired here with optimistic certification;
the default remains single-version lock-based concurrency control. Storage
selection and persistent-GC policy remain independent. Historical versions
stay in sidecars behind logical object/granule identity and are not exposed as
first-class `ObjectTable` slots or persistent-GC roots. Version pruning remains
separate MVCC work.
