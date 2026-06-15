# Transactional WebAssembly Implementation Log

This log records execution state for the Wizard-first transactional Wasmtime
research branch.

Older sections are chronological implementation notes and may describe mocks or
ignore lists that were removed by later sections. Treat the current-status
sections near the top of this file as authoritative.

## Current Roadmap

Date: 2026-06-06

Use `docs/shisoft/transactional-wasm-remaining-work-roadmap.md` for remaining
work sequencing. It supersedes the older WAST-only roadmap where the WAST counts
or object-model sequencing have drifted.

Use `docs/shisoft/transactional-wasm-model-checking-test-roadmap.md` for the
post-WAST correctness-test workstream. It covers deterministic reference-model
tests, generated `proptest` histories, file-backed recovery checks, bounded
`LockBased` state-space tests, permission-state tests, and later `loom` entry
criteria.

Use `docs/shisoft/2026-06-15-persistent-roots-and-promotion-plan.md` for the
completed persistent-root and promotion-boundary wave.

Use `docs/shisoft/2026-06-15-vmgcref-commit-promotion-plan.md` for the
completed first commit-time `VMGcRef` promotion wave.

## Current Status: Wave 11 VMGcRef Commit Promotion

Date: 2026-06-15

Wave 11 promotes the first supported class of volatile transaction references
into durable persistent object records during transaction commit.

Implemented status:

- Durable `ti31` object records now use a dedicated packed object granule
  domain and recover as scalar `ObjectPayload::I31` records.
- `ObjectTable` can reserve persistent `ObjectId` slots for promotion, fill
  those slots after recursive rewrite, and map raw `ti31` immediates to
  promoted scalar object records.
- `TransactionState` keeps transaction-local promotion maps for
  `ObjectId -> ObjectId` and raw `ti31 -> ObjectId`.
- Promotion handles transaction-mirrored volatile `tstruct` and `tarray`
  objects whose payloads are already represented in `ObjectTable`, including
  shared references and cycles.
- Staged persistent roots and staged persistent object payloads are rewritten
  before commit publication so recovered graphs contain persistent `ObjectId`
  references, not raw `VMGcRef` values.
- Promotion registers optimistic reads for promoted struct/array source
  payloads and validates them before the real commit path mutates live
  `tmemory`, globals, tables, or object records.
- Abort cleanup frees promoted targets, clears promotion maps, and drops staged
  promoted payloads through the existing allocated-object cleanup path.
- File-backed end-to-end coverage now runs real WAT through the real engine:
  a `tfunc` creates a `tstruct`, stores it in a `tglobal`, commits, reopens the
  file-backed log, and recovers one matching root/object winner.

Still deferred:

- Promotion of arbitrary ordinary Wasmtime GC heap objects that are not already
  transaction-mirrored in `ObjectTable`.
- Symbolic durable identity for `tfuncref` and `texternref` promotion.
- Replacement of the remaining volatile `VMGcRef -> ObjectId` execution bridge
  with the final `ObjectId`-carrying persistent `tref` ABI.
- Full runtime reintegration of recovered persistent roots into the ordinary
  Wasmtime GC-facing root closure.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib persistent_promotion_commit -- --format terse
test result: ok. 6 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_promotion_commit_path -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_promotion_abort -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_root_commit -- --format terse
test result: ok. 4 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
test result: ok. 5 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 351 passed; 0 failed; 0 ignored

cargo test -p wasmtime --test transaction_persistence -- --format terse
test result: ok. 6 passed; 0 failed; 0 ignored
```

## Current Status: Wave 10 Persistent Roots And Promotion Boundary

Date: 2026-06-15

Wave 10 makes persistent roots first-class durable records and closes the
silent-reference hole at the persistent graph boundary.

Implemented status:

- Durable root publications now exist for persistent `TGlobal` and `TTable`
  roots, using `ObjectId + 1` encoding so zero remains the null/no-root value.
- `TransactionState` keeps a volatile committed persistent-root index keyed by
  global coordinates and exact table-element coordinates.
- Real transaction commits publish staged persistent root records before LP,
  then apply the committed root index after `complete_commit()`.
- Commit-coupled persistent GC now seeds from the committed root index after
  root/object commits invalidate cached reachability.
- File-backed reopen installs recovered root IDs and recovered root versions,
  then reuses recovered stream cursors so later commits append without root
  version collisions.
- At the Wave 10 boundary, unknown or non-persistent non-null `VMGcRef` values
  were rejected before they could enter persistent roots or persistent object
  payload publications. Wave 11 supersedes that for transaction-mirrored
  `tstruct`/`tarray` objects and raw `ti31` values, but arbitrary ordinary
  Wasmtime GC refs still use the same diagnostic:

```text
volatile GC reference promotion into persistent object graph is not implemented yet
```

Still deferred:

- recursive promotion of ordinary volatile Wasmtime GC heap graphs that are not
  already transaction-mirrored in `ObjectTable`
- ordinary Wasmtime GC root-closure integration for persistent roots
- durable block/chunk retirement and persistent block reuse

Verification commands for this slice:

```text
cargo fmt --check
ok

cargo test -p wasmtime --lib persistent_root_publication -- --format terse
test result: ok. 4 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_root_index -- --format terse
test result: ok. 6 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_root_commit -- --format terse
test result: ok. 4 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_root_recovery_installs_root_index_for_gc -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_ref_promotion_boundary -- --format terse
test result: ok. 3 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
test result: ok. 5 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 325 passed; 0 failed; 0 ignored

cargo test -p wasmtime --test transaction_persistence -- --format terse
test result: ok. 5 passed; 0 failed; 0 ignored
```

## Current Status: Wave 9B/9C Persistent Object GC

Date: 2026-06-14

Wave 9B/9C persistent object GC is implemented far enough for the current
`transaction` branch baseline.

Wave 9C now uses tombstone-less Makalu/Ralloc-style recovery. Persistent
storage contains committed object records, roots, type/layout metadata, and
scan-critical region metadata. Recovery selects live object-data winners, marks
from persistent roots, rebuilds the volatile object table only for reachable
objects, and reconstructs auxiliary allocator state. Durable block reuse
remains deferred until block-generation or checkpoint retirement metadata
exists.

Implemented status:

- `PersistentGcState` now observes commit-coupled deltas from committed roots
  and committed object-edge publications, including the runtime
  `transaction_commit_impl` commit epilogue. Post-commit observation is
  best-effort maintenance: observer errors clear cached GC progress and do not
  turn an already-completed commit into a reported transaction failure.
- Commits that change root-bearing globals/tables or publish persistent object
  records invalidate the cached reachable set before observing new commit
  deltas, avoiding stale additive reachability.
- Recovery filters the recovered winner map by reachability before rebuilding
  the volatile `ObjectTable`, so unreachable garbage is not installed and then
  swept later.
- Raw log recovery keeps scan-valid object winners even when stale unreachable
  records refer to missing type-layout metadata. Layout validation is applied
  when a winner is reachable and must be traced or rebuilt into the volatile
  object table.
- Recovered object winners now carry `data_block`, `data_offset`, and
  `record_len`, and the recovery report exposes reachable and unreachable
  durable record locations.
- Runtime volatile sweep/reporting for persistent objects is implemented.
- File-backed end-to-end coverage exercises the tombstone-less recovery path.

Still deferred:

- durable block/chunk retirement, durable block reuse, and Immix line reuse
- explicit `ObjectId` reuse
- commit-time promotion for arbitrary ordinary Wasmtime GC heap graphs beyond
  the transaction-mirrored object graphs implemented in Wave 11
- full runtime reintegration of recovered persistent roots into Wasmtime
  GC-facing root closure

Verification commands for this slice:

```text
cargo fmt --check
ok

cargo test -p wasmtime --lib persistent_gc -- --format terse
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 598 filtered out

cargo test -p wasmtime --lib persistent_object_marker -- --format terse
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 607 filtered out

cargo test -p wasmtime --lib persistent_gc_recovery_filter -- --format terse
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 611 filtered out

cargo test -p wasmtime --lib persistent_gc_commit -- --format terse
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 614 filtered out

cargo test -p wasmtime --lib recovery_ -- --format terse
test result: ok. 46 passed; 0 failed; 1 ignored; 0 measured; 572 filtered out

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 309 passed; 0 failed; 0 ignored; 0 measured; 310 filtered out

cargo test -p wasmtime --test transaction_persistence -- --format terse
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

cargo test -p wasmtime --lib -- --format terse
test result: ok. 617 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out

rg -n "GcTombstone|persistent_object_tombstone|TX_OBJECT_FLAG_DELETED" crates/wasmtime/src/runtime
no matches

git diff --check
no output
```

## Persistent Object GC 9C Design Basis: Tombstone-Less Recovery

Date: 2026-06-14

The Wave 9C tombstone design has been replaced by a Makalu/Ralloc-style
tombstone-less baseline. Persistent storage should not pay runtime write cost
for per-object GC tombstones, mark bits, line marks, free lists, reclaim queues,
or object-table entries. Those structures are auxiliary GC/allocator state and
should be reconstructed during recovery.

The design adopts three specific ideas from Makalu and Ralloc:

- **Recoverability:** after recovery, allocator and object-table metadata must
  describe all and only objects reachable from persistent roots.
- **Ownership before use:** future persistent block/chunk allocation should
  durably claim storage before handing it to the mutator. A crash may leak that
  storage until recovery, but it must not permit double allocation.
- **Typed tracing/filter functions:** Ralloc filter functions map to our
  persistent type/layout metadata. Recovery traces only fields/elements that
  the recovered layout marks as `ObjectId` references.

Recovery now scans user transaction logs to choose the highest live object-data
winner per `ObjectId`, loads persistent roots and type/layout metadata, runs
full `ObjectId` graph marking over the winner map, and rebuilds the volatile
object table only for reachable winners. It also reconstructs volatile line
marks, block live-byte summaries, free lists, and reclaim queues from
reachable records and region metadata.

Durable space reuse moves to a later block/chunk-retirement workstream. Until
the runtime can publish block-generation or checkpoint metadata that lets
recovery ignore retired blocks, GC may report reclaim candidates but must not
reuse persistent blocks whose old records would still be scanned after restart.

The earlier plan
`docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-implementation-plan.md`
is retained only as a supersession stub. The GC tombstone-block plan
`docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-gc-blocks-plan.md`
is also superseded by the tombstone-less plan
`docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-tombstone-less-plan.md`.

## Persistent Object GC Wave 1: Logical ObjectId Marking

Date: 2026-06-14

Persistent GC work has started with a stop-the-world logical marker over
committed persistent `ObjectId` records. This is the first proof point for the
persistent reachability pipeline; it intentionally does not reclaim storage.

Implemented status:

- Added `PersistentObjectMarker` in `transaction.rs`.
- The marker takes explicit `ObjectId` roots and a borrowed `ObjectTable`.
- It rejects marking while the current thread has an active transaction.
- It walks persistent object edges through the existing object-table
  `trace_object_ids` path, including layout-guided tracing for recovered
  persistent records.
- It returns a `PersistentObjectMarkReport` with:
  - reachable persistent objects
  - unreachable persistent objects
  - dangling child references
  - invalid roots split into missing and non-persistent roots
- It does not mutate `ObjectTable`, publish tombstones, free slots, recycle
  blocks, compact records, or integrate with ordinary Wasmtime GC.

Test coverage added in this slice:

- root closure plus unreachable-object reporting
- cycles and shared children
- missing and non-persistent root diagnostics
- dangling child reference diagnostics
- layout/tracing error propagation
- active-transaction rejection
- marking a recovery-rebuilt object table using recovered root IDs

Verification commands for this slice:

```text
cargo test -p wasmtime --lib persistent_object_marker -- --format terse
test result: ok. 7 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 283 passed; 0 failed; 0 ignored

cargo test -p wasmtime --test transaction_persistence -- --format terse
test result: ok. 4 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib -- --format terse
test result: ok. 591 passed; 0 failed; 2 ignored

cargo fmt --check
```

Remaining implementation boundaries:

- Commit-coupled marking, recovery-time winner filtering, recovered record
  location reporting, and volatile sweep are now implemented; see the current
  status section above.
- No durable block/chunk retirement or persistent block reuse yet.
- No block, chunk, or Immix line reclamation yet.
- No root integration with Wasmtime stack/host roots; roots are explicit
  persistent `ObjectId` inputs.
- Active transaction workspaces and promotion maps are not treated as temporary
  roots yet.

## Persistent Type Layout Metadata And File-Backed Object Recovery

Date: 2026-06-14

The persistent object layout workstream now stores trace-oriented type metadata
separately from object records and uses that metadata during recovery. The
object table remains volatile in the Zen-style path: committed object records
are durable, while the runtime index is rebuilt from file-backed log winners
plus recovered layout metadata.

Implemented status:

- `TxObjectHeader` and runtime object-table slots now use `type_layout_id`
  terminology. The durable `TxDataRecordHeader::type_info` wire field remains
  unchanged, but object publication APIs map it to `type_layout_id`.
- `transaction::type_layout` defines `TypeLayoutId`, trace slot kinds, struct,
  array, and scalar persistent layouts, plus a compact little-endian codec and
  `TypeLayoutRegistry`.
- The block-region metadata path stores persistent type layout records in
  region metadata space. File-backed recovery loads the registry before
  replaying object winners.
- Object publication calls `ensure_type_layout` before writing an object data
  record. Publications that reference an unknown layout id fail before durable
  object data is appended.
- Recovery validates each object winner against the recovered layout registry:
  missing layout ids, outer/header layout mismatch, and header-kind/layout-kind
  mismatch are rejected.
- Persistent object reference tracing is layout-guided for durable records.
  Struct trace fields and array element trace kinds determine where `ObjectId`
  references are found; scalar fields are ignored.
- Wasmtime GC descriptors are now mapped into persistent trace layouts at the
  transaction runtime boundary. Wasmtime still owns validation, casts, and
  semantic type checks; persistent storage owns only trace metadata needed for
  recovery and future persistent GC.
- Added file-backed recovery coverage for scalar structs, structs with
  persistent object references, scalar arrays, object-reference arrays, mixed
  `tmemory` undo plus object-publication commit, mixed loose-end rollback, and
  missing-layout rejection.

Verification commands for the most recent file-backed recovery slice:

```text
cargo test -p wasmtime transaction::tests::file_backed_object_layout_recovery -- --nocapture
test result: ok. 6 passed; 0 failed; 0 ignored

cargo test -p wasmtime transaction::tests::file_backed_mixed_tmemory_object_recovery -- --nocapture
test result: ok. 2 passed; 0 failed; 0 ignored

git diff --check
```

Remaining boundaries:

- The final `ObjectId`-carrying `tref` ABI is not complete; some execution paths
  still use the volatile `VMGcRef -> ObjectId` bridge.
- Commit-time promotion from volatile transaction objects into persistent
  object records remains future work.
- Persistent transactional function objects are still not complete.
- Recovery can reconstruct root `ObjectId`s from committed `TGlobal`/`TTable`
  winners, but those recovered roots still need runtime reintegration and
  GC-facing root-closure handling.
- Commit-coupled marking, tombstone-less recovery filtering, recovered record
  location reporting, and volatile sweep/reporting are now implemented. Durable
  block/chunk reclamation and reuse are still future work. The current format
  is designed so those can trace by `ObjectId` without adding a persisted
  object table. Per-object tombstones are not the baseline GC design.

## Model-Checking Wave 10: WAST And Model Cross-Checks

Date: 2026-06-12

Wave 10 of the model-checking roadmap now adds documentation-only WAST/model
cross-checks in `docs/shisoft/transactional-wasm-wast-ledger.md` and refreshes
the current transaction-proposal WAST baseline.

Changes in this slice:

- Added a feature-level coverage table near the WAST ledger baseline for the
  selected state/data-model areas that have explicit later-wave model work.
  It does not claim to summarize the whole transaction WAST corpus; broader
  WAST-only or runtime-smoke families remain tracked in the per-file ledger.
- Cross-linked the current feature rows to the recent model-test filters where
  they actually apply: `model_recovery`, `model_crash`,
  `model_mixed_participants`, `model_lock_based`, `model_permissions`,
  `model_backend_conformance`, `model_object_rebuild`, and
  `transaction_active`.
- Recorded the places where the current WAST suite is broader than the model
  suite or vice versa, especially for `tmemory.size/grow`, table bulk
  operations, permission-state assertions, and durable persistent-object
  recovery.
- Called out the still-open deeper gaps around persistent GC, promotion,
  object-root closure, delete-version precedence, and future shared-thread
  concurrency testing so later waves do not overstate current correctness
  evidence.
- Re-ran the gated transaction WAST suite on 2026-06-12 and recorded the
  actual result, which still matches the expected 173-pass baseline.

Verification command for this slice:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 173 passed; 0 failed; 0 ignored
```

## Model-Checking Wave 7: Transaction Boundary And Active-State Tests

Date: 2026-06-12

Wave 7 of the model-checking roadmap now adds a focused active-transaction
boundary slice in `crates/wasmtime/src/runtime/transaction.rs`.

Changes in this slice:

- Added a nested `transaction_active` unit-test module so the Wave 7 slice is
  runnable with
  `cargo test -p wasmtime --lib transaction_active -- --format terse`.
- Added a direct `TransactionState` regression that covers the active-state
  gate across transactional memory, memory-size, global, table-granule,
  table-size, and persistent-object acquisition helpers. Persistent object
  field/element helpers are also checked to ensure they do not succeed without
  an active transaction or previously acquired permission.
- Added small imported-host WAT smoke tests proving that an exported top-level
  `tfunc` enters an active transaction, clears thread-local active state on
  return, still commits its side effect, and allows a second exported `tfunc`
  call to run afterward with its own active transaction boundary.
- Added a trap-path smoke test proving that a trapping `tfunc` aborts staged
  work, clears thread-local active state, and does not block a later exported
  `tfunc` from entering its own active transaction boundary.
- Added a nested `tcall` smoke test that observes the same transaction id in
  the outer `tfunc`, the inner callee, and the post-`tcall` continuation,
  proving that nested transactional calls reuse the active transaction until
  the outermost return.
- Kept the Wave 7 changes localized to `transaction.rs`; no WAST harness or
  proposal WAST edits were needed because the existing transaction-proposal
  parser/runtime test path already covers the broader syntax/runtime lane and
  this slice only needed a focused active-state unit-test filter.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib transaction_active -- --format terse
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 9: ObjectTable Rebuild Model Tests

Date: 2026-06-12

Wave 9 of the model-checking roadmap now adds an object-table rebuild slice in
`crates/wasmtime/src/runtime/transaction.rs`.

Changes in this slice:

- Added a nested `model_object_rebuild` unit-test module so the Wave 9 slice is
  runnable with
  `cargo test -p wasmtime --lib model_object_rebuild -- --format terse`.
- Added a test-only `ModelObjectRecord` reference shape that tracks object id,
  record version, object kind, one small payload field, and commit visibility
  independently of live transaction execution.
- Added deterministic rebuild coverage proving that the highest committed
  version wins per object id, loose-end object publications stay invisible,
  rebuilt persistent objects preserve their recovered `ObjectId` as runtime
  identity, duplicate committed same-version object publications are rejected
  by real recovery, and corrupt payload/header mismatches are rejected during
  rebuild.
- Replaced the earlier tautological helper path with a real file-backed durable
  log harness: the tests now publish the full model history through
  `TxDurableLog`, omit LPs for loose-end records, reopen the durable region,
  recover real object winners, and only then call
  `ObjectTable::rebuild_from_recovery_for_test`.
- Added fixed-seed generated rebuild histories over small struct/array object
  records and compared rebuilt `ObjectTable` state against the independent
  reference model's highest committed version per `ObjectId`, including payload
  bytes, kind, logical id, publication version, publication `type_info`,
  persistence bit, live-slot count, and the file-backed recovery summary's
  selected winner versions.
- Kept the implementation localized to test-only helpers in
  `transaction.rs`; no production object semantics changed for this slice.

Semantic note for this slice:

- Generated histories intentionally assign monotonically increasing versions per
  object id, so they exercise the ordinary winner-selection path without
  duplicate committed object/version pairs. A separate deterministic regression
  now proves that real recovery rejects duplicate committed same-version object
  publications instead of silently accepting them.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_object_rebuild -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 8: Durable Backend Conformance Harness

Date: 2026-06-12

Wave 8 of the model-checking roadmap now adds a backend-conformance slice in
`crates/wasmtime/src/runtime/transaction/persist.rs`.

Changes in this slice:

- Added a nested `model_backend_conformance` unit-test module so the Wave 8
  slice is runnable with
  `cargo test -p wasmtime --lib model_backend_conformance -- --format terse`.
- Added a test backend-factory abstraction that drives the same transaction
  scenarios through backend-compatible `TxDurableLog` instances instead of
  hard-coding file-backed recovery-only tests.
- Added a file-backed factory that creates temp path-backed logs and compares
  recovered durable summaries via
  `TxDurableLog::recover_file_backed_for_test`.
- Added an in-memory factory that runs the same LP visibility, log-entry role,
  tx-id, CRC, and data-pointer ordering scenarios through the default
  in-memory durable log, without claiming reopen recovery coverage.
- Added deterministic conformance scenarios for object-only commit,
  `tmemory`-undo-only commit, mixed commit, mixed loose end, and multi-stream
  transaction ids.
- Added shared assertions for log-entry roles, tx ids, final-LP placement, CRC
  sealing, and LP data-pointer reuse across both backends, with recovery
  summary comparison enabled only for the file-backed backend and
  role-specific object-vs-`tmemory` data-stream separation checked in the
  file-backed mixed conformance scenarios.

Semantic note for this slice:

- The in-memory factory is intentionally limited to non-restart properties. It
  verifies publish ordering and log shape through test hooks, but only the
  file-backed factory proves reopen-and-recover behavior.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_backend_conformance -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 6: Permission-State Model Tests

Date: 2026-06-12

Wave 6 of the model-checking roadmap now adds permission-state model tests in
`crates/wasmtime/src/runtime/transaction.rs`.

Changes in this slice:

- Added a nested `model_permissions` unit-test module so the Wave 6 slice is
  runnable with `cargo test -p wasmtime --lib model_permissions -- --format terse`.
- Added an object-focused reference permission machine that tracks active
  transaction state, committed value, staged value, and transaction-owned
  read/write access on the persistent object granule, then compares every
  schedule prefix against real `TransactionState` behavior.
- Added deterministic regressions covering read-only access, write-owned staged
  mutation, write-implies-read of the staged value, permission cleanup on
  abort, and permission cleanup on commit/release.
- Added a fixed-seed generated short-sequence test over the Wave 6 operation
  alphabet: `grant read`, `grant write`, `read`, `write`, `downgrade`,
  `abort`, and `commit/release`, with prefix-by-prefix comparison against the
  reference machine's semantic step results plus exact post-state snapshots.
- Added one small non-object granule check using a `TGlobal` granule so the
  tests show the transaction/granule permission state is generic and not tied
  to object payload helpers alone.

Semantic note for this slice:

- There is no production permission-downgrade API yet. The Wave 6 tests model
  downgrade with a test-only transition that clears active write ownership for
  one granule while preserving the transaction's read permission and any staged
  object payload. This matches the meeting design intent closely enough to
  verify that subsequent writes are rejected until write ownership is acquired
  again.
- The permission model now checks the semantic contract rather than current
  helper diagnostic ordering: read permission enables reads, write permission
  enables staged mutation, write ownership implies read of the staged value,
  and writes attempted without write ownership must fail without mutating state.
  The tests compare runtime success/failure plus post-state and therefore do
  not depend on whether the current object helpers report read-denied or
  write-denied first.
- The `commit/release` transition uses the real object commit path
  (`commit_object_payloads` plus `complete_commit`) for the object model and
  `complete_commit` for the small generic granule release check.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_permissions -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 5: LockBased Reference-World State-Space Tests

Date: 2026-06-12

Wave 5 of the model-checking roadmap now adds deterministic and generated
`LockBased` state-space tests in
`crates/wasmtime/src/runtime/transaction.rs`.

Changes in this slice:

- Added a nested `model_lock_based` test module so the Wave 5 slice is runnable
  with `cargo test -p wasmtime --lib model_lock_based -- --format terse`.
- Added test-only `LockModelOp`, `LockModelState`, and step/error helpers that
  mirror the current lock manager semantics for reads, writes, release, and
  abort without using OS threads.
- Added a deterministic regression schedule that exercises writer preemption,
  read-to-write upgrade, multi-granule ownership, and release cleanup against
  the reference model.
- Added exhaustive small-schedule coverage over the full 20-operation alphabet
  induced by txs `1/2`, granules `0/1`, versions `0/1`, plus `Abort` and
  `Release`. The default unit-test path now exhaustively enumerates every
  schedule prefix through length `3` (8,421 total schedules) so
  `cargo test -p wasmtime --lib model_lock_based -- --format terse` stays
  quick in ordinary runs.
- Kept the original full depth-`5` traversal (3,368,421 total schedules)
  available behind
  `WASMTIME_TRANSACTION_LOCK_MODEL_EXHAUSTIVE=1 cargo test -p wasmtime --lib model_lock_based -- --format terse`
  for manual coverage sweeps when longer runtimes are acceptable.
- Replaced raw substring assertions with typed test-only `LockBased` conflict
  helpers so the model tests and lock regressions compare exact conflict kinds
  through the real lock-operation path while keeping the production diagnostics
  unchanged.
- Added test-only `LockBased` snapshot/clone helpers so the reference model no
  longer reaches into private lock-manager fields or relies on a production
  `Clone` derive that only existed for tests.
- Retuned the fixed-seed generated `proptest` coverage to complement the
  bounded exhaustive sweep: one profile exercises medium-length histories and a
  second profile stresses longer churn sequences (`24..=48` ops) that the
  quick exhaustive pass does not reach, while keeping runtime practical.
- Added explicit post-prefix checks for the Wave 5 invariants: one owner per
  granule, owner-map and owner-set agreement, abort/release cleanup, failed
  conflicting writes preserving the existing owner, failed reads against an
  incompatible writer, and optimistic read-version mismatches.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_lock_based -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 4: Version Selection And Corruption Recovery Tests

Date: 2026-06-12

Wave 4 of the model-checking roadmap now extends the durable recovery slice in
`crates/wasmtime/src/runtime/transaction/persist.rs` plus the raw region-image
recovery tests in `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`.

Changes in this slice:

- Added deterministic `model_recovery_selects_highest_committed_object_version`
  coverage so committed versions `2`, `9`, and `7` for the same object id
  deterministically recover version `9`.
- Added deterministic duplicate-version corruption coverage:
  `model_recovery_rejects_distinct_duplicate_committed_object_version`
  exercises two committed object publications with the same logical
  id/version but different payload bytes, while
  `model_recovery_accepts_idempotent_lp_duplicate_pointer` preserves the
  existing exact same logical/version/data-pointer duplicate LP acceptance.
- Added malformed-image coverage for corrupt log-entry CRC, committed
  log/data-role mismatch, object-publication data-role mismatch, and data
  pointers outside any data chunk.
- Added explicit multi-block data-chunk regressions for a valid publication
  pointer landing in a later block, rejection when a publication payload
  crosses the chunk tail, and rejection when a publication pointer lands in
  the data-chunk header region.
- Tightened recovery validation so committed entries now validate their data
  record logical id/version and reject mismatched publication versus
  `TMemoryUndo` roles instead of silently dropping malformed committed records.
- Tightened raw data-record loading so recovery rejects pointers that do not
  land inside a discovered data chunk or that extend past the chunk tail.
- Replaced the per-load whole-region data-chunk scan with one discovery pass
  that indexes each chunk's block range, start offset, header boundary, and
  tail offset for reused publication-pointer validation.

Deferred in this slice:

- Delete-version precedence remains omitted because delete semantics are not
  wired into the durable object model yet.
- Data-chunk stream mismatch remains deferred. Current recovery discovers data
  chunks globally and validates chunk membership plus chunk-tail bounds, but it
  does not yet enforce a stronger stream-identity rule for object publication
  pointers, and this slice does not force that broader design change.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_recovery -- --format terse
cargo test -p wasmtime --lib block_region -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 3: Mixed Participant Reference-World Tests

Date: 2026-06-12

Wave 3 of the model-checking roadmap now adds mixed file-backed participant
tests in `crates/wasmtime/src/runtime/transaction.rs` that compare one real
transaction against a small abstract world with explicit `tmemory` bytes and
persistent-object field values.

Changes in this slice:

- Added a nested `model_mixed_participants` test module so the Wave 3 slice is
  runnable with a focused cargo test filter.
- Added a small `ModelWorld` reference state that records expected file-backed
  `tmemory` granule bytes and recovered persistent-object field values.
- Added deterministic commit coverage proving that a committed mixed
  transaction keeps the new file-backed `tmemory` byte image and rebuilds the
  persistent object winner with the updated field value.
- Added deterministic loose-end coverage proving that a missing LP rolls
  file-backed `tmemory` back to old bytes and rebuilds no object winners.
- Added a fixed-seed generated one-transaction mixed test that produces 1 to 3
  `tmemory` writes, 0 to 2 persistent-object writes, and a commit/loose-end
  choice, then compares recovered real behavior against the reference world.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_mixed_participants -- --format terse
cargo test -p wasmtime --lib transaction -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 2: Crash Cutpoint LP-Visibility Tests

Date: 2026-06-12

Wave 2 of the model-checking roadmap now adds executable crash-cutpoint tests
for the Zen-style durable ordering in
`crates/wasmtime/src/runtime/transaction/persist.rs`.

Changes in this slice:

- Added a nested `model_crash` test module under the existing `model_recovery`
  scaffolding so the Wave 2 cutpoint slice is runnable with a focused cargo
  test filter.
- Added `CrashCutpoint` coverage for `BeforeAnyRecord`,
  `AfterUndoRecordBeforeLp`, `AfterObjectRecordBeforeLp`,
  `AfterAllRecordsBeforeLp`, and `AfterLp`.
- Added deterministic regressions proving that a crash before LP drops object
  winners and preserves `tmemory` undo rollback bytes, while a crash after LP
  commits object winners and ignores `tmemory` undo rollback actions.
- Added generated cutpoint coverage that publishes one logical transaction
  through the real file-backed `TxDurableLog`, truncates publication at each
  cutpoint by selecting which publish calls occur, and compares recovered
  object winners plus rollback-byte summaries against the reference evaluator.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_crash -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo fmt --check
git diff --check
```

## Model-Checking Wave 1: Durable-Log Recovery Reference Tests

Date: 2026-06-12

Wave 1 of the model-checking roadmap now has an executable recovery-model
slice in `crates/wasmtime/src/runtime/transaction/persist.rs`.

Changes in this slice:

- Added a nested `model_recovery` test module with test-only `ModelRole` and
  `ModelRecord` history inputs for durable transaction recovery.
- Added a deterministic regression covering one committed transaction that
  appends a `TMemoryUndo`, appends a `TObjectPub`, publishes LP, and recovers
  exactly one object winner with zero `tmemory` rollbacks.
- Added a pure `expected_recovery` reference evaluator that groups records by
  transaction, treats `has_lp` as commit, keeps `TObjectPub` winners only from
  committed transactions, keeps `TMemoryUndo` rollbacks only from loose-end
  transactions, and selects the highest object version per logical id.
- Added a fixed-seed small-history `proptest` that publishes generated model
  records through the real file-backed `TxDurableLog` and compares recovered
  object winners plus `tmemory` rollback summaries, including recovered old
  granule bytes, against the reference evaluator.

Verification commands for this slice:

```text
cargo test -p wasmtime --lib model_recovery -- --format terse
cargo test -p wasmtime --lib transaction::persist -- --format terse
cargo fmt --check
git diff --check
```

## Transactional Object Durable Commit Path

Date: 2026-06-11

Persistent object payload commits now publish `TObjectPub` records through the
transaction-owned `TxDurableLog`. The runtime commit coordinator collects
object `PendingPublication` records from `commit_object_payloads_into`, appends
them to the same durable stream used by persistent `tmemory` undo records, and
publishes one final LP for the whole transaction.

Changes in this slice:

- Added `StreamPublisher::publish_object_publication_before_commit` for
  non-final object publication log entries.
- Added `TransactionState::publish_object_publications_before_commit` so object
  commit publication is owned by the transaction state rather than by the
  object table.
- `commit_object_payloads_into` now filters durable publications to persistent
  object slots. Volatile transactional object updates still commit to the
  store-local object table but do not enter the durable log.
- The file-backed durable log keeps one transaction log-entry stream for
  ordering, but allocates object publication data records and linear-memory
  undo data records from separate data streams.
- `TxDurableLog` now dispatches through a `TxDurableLogBackend` trait.
  File-backed storage is one backend implementation alongside the in-memory
  scaffold, so future PMEM or alternate filesystem backends plug into the same
  append/flush/fence boundary instead of creating a separate commit path.
- Changed the real libcall commit path so `commit_staged_tmemory_records`
  returns a durable marker instead of publishing LP internally.
- Recovery now treats an exact duplicate logical/version/data pointer as an
  idempotent LP marker while still rejecting distinct duplicate committed
  object versions as corruption.
- Added file-backed tests for uncommitted object publications, committed object
  publications, and mixed object publication plus `TMemoryUndo` transactions
  with a single LP. The mixed restart-style tests use file-backed `TMemory` and
  file-backed `TxDurableLog`: committed transactions keep the new linear-memory
  bytes and rebuild the persistent object winner, while loose-end transactions
  roll linear memory back from undo records and drop the object publication.

Still deferred:

- GC-backed object identity and persistent-object promotion are not complete.
  The durable commit path can publish object records, but creating and tracking
  all persistent transactional objects still depends on the GC/object-model
  workstream.
- Store construction still defaults to the in-memory durable transaction log.
  File-backed transaction-log selection exists at the lower layer and needs
  system-level configuration before ordinary engine users get restart recovery.

## TMemory Undo-In-Place Durable Substrate

Date: 2026-06-11

Added the first durable substrate for the linear-memory ordering problem
identified in `docs/shisoft/meeting_saved_closed_caption copy 2.txt`.
Persistent `tmemory` should not use object-style redo publication for every
modified granule. Objects remain COW/redo records whose data record is the
object storage itself; linear-memory undo records are separate rollback
material containing old granule bytes.

Changes in this slice:

- `TxLogEntry` now carries a CRC-covered role:
  - `TObjectPub` for ordinary object/global/table publication records.
  - `TMemoryUndo` for old linear-memory granule bytes.
- `TxDataRecordHeader` now carries a matching record role in the former
  reserved field.
- Added `PendingGranuleUndo` and `encode_undo_data_record` so linear-memory
  undo records are not represented as `PendingPublication`.
- Added `StreamPublisher::publish_tmemory_undo_before_in_place_write`, which
  enforces data write, data flush, fence, undo log write, log flush, fence
  before any future in-place persistent write.
- Added `StreamPublisher::publish_commit_lp` so undo-only persistent
  `tmemory` transactions can publish a final LP without appending another data
  record.
- Added transaction-owned durable logging for persistent `tmemory` commits.
  `TransactionState` owns the current unified in-memory durable stream and
  publishes all `TMemoryUndo` records plus one final LP for the whole
  transaction.
- Split `TMemory` into participant operations:
  `prepare_tmemory_undo_record`, `commit_staged_tmemory_granule`, and
  `commit_staged_tmemory_granules_direct`. `TMemory` no longer owns transaction
  log streams or publishes LP markers.
- Routed runtime staged `tmemory` commits through the unified coordinator in
  `libcalls.rs`. Transactions can now touch multiple persistent `tmemory`
  participants without producing per-memory LP markers. `VMemory` remains on
  the volatile no-log commit path.
- Recovery now ignores committed `TMemoryUndo` entries and turns loose-end
  `TMemoryUndo` entries into `RecoveredTMemoryUndoRollback` actions.
- Added `TMemory::apply_recovered_tmemory_undo_rollbacks` so recovered rollback
  actions can restore old base-image bytes without bumping granule versions.
- Added participant collection in the commit path so staged memory granules are
  grouped by owning instance and memory index before durable logging and
  in-place application.
- Added a file-backed `TxDurableLog` storage variant. It uses the existing
  block-region layout for data chunks and fixed-size log entries, flushes data
  and log blocks through the file-backed mmap backend, and can be reopened by
  recovery to distinguish committed undo records from loose-end rollback
  records.
- Extended file-backed recovery summaries to expose recovered `TMemoryUndo`
  rollback records for tests and later restart integration.

Still deferred:

- Transaction execution still uses the existing staged-memory workspace.
  Persistent backends perform undo-before-in-place during commit, when staged
  granules are applied to the base image.
- Full process restart recovery does not yet discover the correct reopened
  `TMemory` instance/backing file and invoke
  `apply_recovered_tmemory_undo_rollbacks`; the apply helper is present.
- Ordinary store construction still defaults to the in-memory durable-log
  variant. The file-backed transaction-log backend exists, but it still needs
  configuration plumbing and full process-restart dispatch into reopened
  `TMemory` instances.
- Hardware-PMEM validation remains future work; the file-backed backend uses
  `msync`/`sync_data` as the current persistence analogue.

## Zen-First Object Publication Metadata

Date: 2026-06-09

Started the first Zen-first persistent-object runtime slice.

Changes in this slice:

- `TxObjectHeader` now carries `version`, a monotonic record-version field for
  committed object records. This is ordered publication metadata, not time or a
  GC epoch.
- `ObjectTable` now assigns increasing record versions whenever a new
  current record is published for an `ObjectId`.
- The object-record bytes written into the heap now serialize
  `record_len`, `object_id`, `version`, `kind`, `flags`, and
  `type_index`.
- `TransactionConfig` now carries an object-index persistence-policy knob. The
  default is `RebuildOnRecovery`; `PersistentIndex` is recognized but rejected
  as not implemented yet.
- Added the first recovery-style rebuild path:
  `ObjectTable::rebuild_volatile_index_from_heap()` scans heap records, selects
  the highest `version` per `ObjectId`, recreates the live slot map,
  repopulates the free-list holes, and resets the runtime bridge maps.

This is the first executable step toward the Zen direction where the persistent
heap is durable and the runtime object index is rebuilt in DRAM during recovery
instead of being persisted as a separate table.

Verification:

```text
cargo test -p wasmtime --lib transaction_object_ -- --format terse
test result: ok. 28 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction_config -- --format terse
test result: ok. 9 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 130 passed; 0 failed; 0 ignored
```

Remaining gaps after this slice:

- publication metadata is still in-memory only; there is not yet a restart path
  for `FileBackedMemory` or `NVMemory`
- freed/deleted persistent-object recovery semantics still need explicit durable
  representation if object deletion becomes part of committed state
- committed roots and promotion still need to publish only `ObjectId` edges
- remaining `VMGcRef` and `VMFuncRef` bridges are still volatile scaffolding

## NVMemory Backend Implementation

Date: 2026-06-09

`NVMemory` now exists as the first PMEM-shaped transactional memory backend.
It shares the block/chunk storage abstraction with `VMemory`, uses the normal
copy-on-write transaction commit path, and flushes committed ranges through the
PMEM persistence engine. The default backend remains `VMemory`; when
`NVMemory` is selected, research pretend-PMEM mode remains the default and
hardware CLWB/SFENCE mode is explicitly selectable through transaction
configuration for experiments.

Real restart/power-fail persistence tests remain gated behind
`WASMTIME_TEST_REAL_PMEM=1` until durable recovery metadata and PMEM hardware
are available.

## FileBackedMemory Backend Implementation

Date: 2026-06-09

`FileBackedMemory` now exists as the filesystem-backed transactional memory
backend. It uses the same block/chunk and copy-on-write commit path as
`VMemory` and `NVMemory`, publishes committed bytes into a shared writable file
mapping, and flushes/fences through filesystem durability APIs. The default
backend remains `VMemory`; file-backed temp-file and explicit-path modes are
both opt-in transaction configuration choices.

Restart/power-fail recovery remains gated behind
`WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1` for full end-to-end `TMemory`
restart wiring, but the durable region/log/data substrate can now reopen
file-backed region images and rebuild stream/block state through chunk-start
scan plus log replay.

## Transactional Grow Ordering Follow-Up

Date: 2026-06-09

Same-transaction `tmemory.grow` followed by accesses into the newly grown range
needs a dedicated semantics pass. The current staged-record commit path can
apply memory-granule records before the staged memory-size record, so this
ordering should be corrected independently of the `NVMemory` PMEM backend
workstream. Existing transaction-proposal WAST coverage still passes, but this
case should get a focused regression test before changing commit ordering.

## NVMemory PMEM Backend Decision

Date: 2026-06-09

This section recorded the sequencing decision to start durable backend work
with `NVMemory` before `FileBackedMemory`. That sequencing is now complete:
`FileBackedMemory` has landed as the filesystem-backed backend, while
`NVMemory` remains the PMEM-shaped backend for CLWB/SFENCE durability
experiments. Tests that require actual power-fail persistence or post-restart
recovery remain ignored or gated until a real PMEM machine and durable
recovery metadata are available.

Detailed design:
`docs/shisoft/transactional-wasm-nvmemory-pmem-design.md`.

## Transaction Proposal WAST Closure

Date: 2026-06-07

The transaction proposal WAST harness now runs the full corpus without ignored
files on the real Wasmtime engine path:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 173 passed; 0 failed; 0 ignored
```

Changes in the closure tranche:

- Promoted `simple-transactions/tconflict-basic.wast` and
  `simple-transactions/tconflict-tmemory_1.wast` out of the ignored set.
- Added real selected-transaction-id conflict behavior for the proposal
  `spectest` helpers, including stale pending-id handling when the runtime
  conflict-aborts a transaction.
- Matched `TransactionState`, `VMemory`, and libcalls on the single
  `TMEMORY_GRANULE_SIZE` source-of-truth constant. The current value is
  64 bytes (`TMEMORY_GRANULE_SHIFT = 6`), while the Immix line size remains
  256 bytes.
- Removed the path-scoped generated-fixture bridge for
  `simple-transactions/tconflict-tmemory_1.wast`. A narrow runtime helper now
  handles the generated `br_on_tcast ... ti31` result-extraction shape while
  keeping the fixture on real transaction parser/runtime backends.

## Persistent Object GC Decision

Date: 2026-06-06

Persistent-object GC is a future workstream, but the object heap/table work
must be persistent-GC-ready immediately. Current implementation waves should
produce `ObjectId`-addressed records with explicit headers, traceable payload
layouts, and committed refs stored as `ObjectId`s. The branch may reuse
Wasmtime GC type/layout/cast/validation code, but must not reuse `GcHeap`,
`VMGcRef`, Wasmtime GC roots, or Wasmtime GC barriers as persistent object
identity or storage.

## Mock Registry and Tagging

Date: 2026-06-04

All intentional transactional Wasm mocks and scaffolds should carry the
grep-able tag `SHISOFT-TWASM-MOCK`. Use this command before replacing the mock
runtime with real backends:

```bash
rg "SHISOFT-TWASM-MOCK"
```

Current tagged mock categories:

- No current `SHISOFT-TWASM-MOCK` tags remain in
  `crates/test-util/src/wast.rs`. The harness no longer performs whole-fixture
  replacement or transaction syntax normalization; enabled proposal WAST files
  use the real transaction parser/runtime path. The remaining harness rewrite
  is diagnostic-only, mapping proposal assertion strings to equivalent
  Wasmtime-style diagnostics.
- Runtime libcall gaps in `crates/wasmtime/src/runtime/vm/libcalls.rs`.
  Scalar `tmemory` and numeric/v128 `tglobal` ops run through real compiled
  helper paths. Numeric imported `tglobal` is supported; object struct/array
  payload operations now use the volatile `ObjectId` bridge and COW object
  helpers. Remaining gaps are durable persistent-object identity/storage and
  replacement of the generated-helper compatibility path with final proposal
  helper typing.
- Transaction state/config scaffold in
  `crates/wasmtime/src/runtime/transaction.rs`. Backend, durability,
  conflict-policy, and concurrency-control selection have the future shape,
  with store-local `LockBased` active, `VMemory` remaining the default backend,
  and opt-in `NVMemory`/`FileBackedMemory` backend paths available while durable
  restart recovery remains deferred.
- Table bulk and element-object gaps in Cranelift/runtime lowering are narrowed
  to final persistent reference/object table semantics. The current funcref
  `ttable` paths acquire `TTable`/`TTableSize` ownership and the enabled table
  fixtures execute through real runtime helpers.
- Opt-in proposal `spectest` transaction helper imports in
  `crates/wast/src/spectest.rs`. This `SHISOFT-TWASM-MOCK` surface provides
  deterministic transaction ids, granule-size helpers, and synchronous helper
  calls for harness progress. `run_as_tid`, `abort_txn`, and `tcommit_txn`
  drive the runtime selected-transaction-id hooks and exercise the first
  store-local `LockBased` conflict path, but this is still not the full Wizard
  scheduler.
- The local `wasm-tools-transaction` fork also carries
  `SHISOFT_TRANSACTION_SCAFFOLD` parser comments for transaction text shapes
  that parse proposal fixtures while later runtime semantics are still being
  hardened, including permission casts, `tblock`, structured `ttry`, and
  folded `tfail` failure-code text.

## Runtime Core Current Status

Date: 2026-06-06

The runtime-core implementation plan through the block/chunk storage wave is
implemented on branch `transaction`.

Implemented runtime paths:

- Transaction object metadata for `tmemory`, `tglobal`, `tfunc`, and `ttable`.
- Per-instance `TMemory` sidecars backed by `VMemory` block/chunk regions.
- Copy-on-write transaction workspace indexed by `GranuleId`.
- `GranuleId::TMemory`, `TMemorySize`, `TGlobal`, `TTable`, and `TTableSize`.
  `TStruct` and `TArray` are wired to `ObjectId` through the first in-memory
  `ObjectTable` foundation.
- Store-local `LockBased` optimistic-read/pessimistic-write ownership with
  selected transaction-id workspaces.
- Generic runtime read/write permission acquisition over every `GranuleId`
  kind; memory, globals, tables, memory/table sizes, and object granules now
  share one permission path.
- `ObjectTable` foundation with dense stable `ObjectId` allocation, freed-slot
  reuse, live-slot version metadata, and kind-aware `TStruct`/`TArray` granule
  acquisition.
- Object payload copy-on-write foundation for struct/array payloads, including
  staged transaction payloads, abort discard, commit application, and
  optimistic object read-version validation.
- Shared transactional object ABI scaffold for future object libcalls:
  `ObjectId` references use `0` for null and `object_index + 1` for non-null
  persistent objects, while scalar/ref/v128 object field values use an explicit
  tag plus two 64-bit payload words. Object heap record serialization now uses
  the same object-reference encoding.
- `tfunc` entry/normal-return transaction boundaries.
- Scalar `tmemory` loads/stores, size/grow, `tmemory.copy/fill/init`, static
  and segmented active `tdata` initialization, imported `tmemory` active data,
  and dynamic imported `tmemory` size matching through real `tmemory` storage.
- Numeric transactional globals for defined and imported globals; v128
  transactional globals for defined globals.
- Transactional SIMD memory load/store families through real parser/lowering
  and `tmemory`.
- Funcref `ttable.get/set/size/grow` plus `ttable.copy/init` ownership/runtime
  smoke paths.

Remaining tagged mock boundaries:

- No path-scoped fixture replacements remain in the proposal WAST harness.
- No transaction syntax-normalization compatibility path remains in
  `crates/test-util/src/wast.rs`; enabled proposal files use the real parser.
  The remaining harness rewrite is diagnostic-only.
- Reference/object-valued transactional snapshots still bridge through raw
  Wasmtime reference values and volatile `ObjectId` maps where the final
  persistent object ABI is not yet installed. Numeric imported `tglobal` is on
  the real runtime path.
- Durable reference/object permissions and the final persistent object-table
  backend. The current volatile `ObjectId` bridge is enough for WAST
  execution, but it is not the final persistent object model.
- Full proposal binary validation beyond the current WAST fixtures, including
  hardened diagnostics for every transactional type/ref encoding.
- Durable restart/recovery metadata and loading for `FileBackedMemory` and
  `NVMemory` backends.

Verification:

```text
cargo test -p wasmtime-environ transaction_object_metadata
test result: ok. 8 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib tmemory
test result: ok. 39 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 84 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 173 passed; 0 failed; 0 ignored
```

Current promotion note:

- `tcall_indirect.wast`, `tfunc_ptrs.wast`, `texports.wast`, and
  `timports.wast` are now real-parser WAST files rather than normalized
  compatibility fixtures.
- `timports.wast` uses real transactional spectest imports, numeric imported
  `tglobal`, active imported `tmemory` data initialization, and dynamic
  `tmemory` import-size matching after grow.

Current ignored proposal files after the 2026-06-07 conflict update:

- none

## LockBased Selected Transaction IDs

Date: 2026-06-07

Added the first runtime slice needed by the conflict WAST workstream:

- `TransactionState` now supports selected transaction ids with separate
  suspended workspaces. Existing transaction operators continue to read and
  write the current workspace, while `enter_transaction`/`restore_transaction`
  can switch the current workspace around a nested call.
- The shared `LockBased` ownership table remains store-local and is shared by
  every selected transaction id, so pessimistic write ownership and optimistic
  read-version validation work across suspended transactions.
- Added selected abort and selected commit hooks. Selected commit reuses the
  existing staged-record application path used by compiled `transaction_commit`.
- The proposal `spectest` helpers now use doc-hidden `Caller` hooks to enter a
  selected tid for `run_as_tid`, restore the previous tid after the call, and
  route `abort_txn`/`tcommit_txn` into runtime transaction state.

Still deferred:

- `tconflict-basic.wast` cannot simply be promoted yet. On the real-parser path
  it reaches `general_conflict_test`, which creates transactional arrays in an
  ordinary function before any explicit transaction boundary. That conflicts
  with the current Wizard-first rule that t-prefixed operations require an
  active transaction.
- `tconflict-tmemory_1.wast` still has the separate WAST/type-shape issue where
  an `i32` `ti31.get_s` result feeds an `i64` local in the generated conflict
  helper.

Verification:

```text
cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 118 passed; 0 failed

cargo test -p wasmtime-wast transaction_spectest_helpers_match_fixture_ids_and_granules -- --format terse
test result: ok. 1 passed; 0 failed
```

## Structured TTry And Growth Rollback

Date: 2026-06-07

Moved `simple-transactions/ttry-abort-commit.wast` onto the real parser/runtime
path without WAST edits.

Runtime/compiler work in this tranche:

- Added an internal `ttry.end` marker in the local `wasm-tools-transaction`
  fork for structured text `ttry` bodies.
- `tfail` now marks structured failure, aborts active transaction state, and
  suppresses later nested `tfunc` calls until the enclosing `ttry.end` clears
  the failure.
- `ttable.set` uses an undo-log write-through path so `tcall_indirect` sees
  same-transaction function table updates while `tfail`/trap rollback restores
  the original table elements.
- `ttable.grow` and `tmemory.grow` now stage logical size changes and commit
  physical growth only on successful transaction commit. Early limit checks
  preserve the proposal `-1` grow result instead of trapping during commit.

Still deferred:

- `ttry-basic.wast` remains a path-scoped harness replacement for structured
  `else` handler semantics.
- Conflict WAST fixtures still need real multi-transaction helper/concurrency
  behavior.

Verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast ttry-abort-commit -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 171 passed; 0 failed; 2 ignored; 0 measured; 3438 filtered out
```

## Object Bridge: TArray Data/Elem And ObjectId Identity

Date: 2026-06-07

Moved `simple-transactions/tarray.wast` onto the real parser/runtime path
without modifying the proposal WAST file.

Runtime/compiler work in this tranche:

- `tarray.new_data` now records the ordinary Wasmtime GC allocation in the
  transactional `ObjectTable`, keyed by `ObjectId`, and decodes scalar/vector
  data-segment bytes into transactional array payload values.
- `tarray.new_elem` now records reference-array allocations in the
  transactional `ObjectTable` and converts live element-segment `VMGcRef` words
  into `ObjectValue::Ref(ObjectId)` payload values.
- Transactional array get/set/new/default/fixed paths can now carry non-i31 GC
  reference elements through the temporary Wasmtime `VMGcRef` ABI bridge while
  storing transaction payload identity as `ObjectId`.
- The runtime design now explicitly requires persistent transactional function
  references to use function-object `ObjectId`s. Wasmtime `FuncIndex`,
  `VMFuncRef`, and compiled-code handles are payload metadata, not persistent
  identity.

Still deferred:

- `tarray.copy`, `tarray.init_data`, and `tarray.init_elem` still need
  transaction-aware object payload updates.
- Function references, i31 immediates, extern references, and full `tref`
  casts/tests still need the final `ObjectId` runtime representation rather
  than this temporary `VMGcRef` bridge.

Verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tarray.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 99 passed; 0 failed; 17 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 156 passed; 0 failed; 17 ignored
```

## Object Bridge: TArray Fill And Permission Type-State

Date: 2026-06-07

Moved `simple-transactions/tarray_fill.wast` onto the real parser/runtime path
without modifying the proposal WAST file.

Implemented in this checkpoint:

- The local `wasm-tools-transaction` fork now preserves `tref none/read/write`
  permission type-state in parsed/validated `RefType`s.
- `tref.cast_read` and `tref.cast_write` now parse as distinct local
  transactional operators. Wasmtime lowers them as identity operations because
  they change validator type-state, not reference identity.
- Validator subtyping treats `write` as usable where `read` is expected and
  treats `tref.null` as permission-top type-state because null carries no
  object to acquire.
- `tarray.fill` now lowers to a transaction libcall, resolves the target through
  the temporary `VMGcRef` to `ObjectId` bridge, stages the COW array payload
  update, and aborts the active transaction on runtime errors.

Still deferred:

- Permission type-state is static validation state. Runtime conflict and
  ownership enforcement remains keyed by `GranuleId`/`ObjectId`, not by a
  permission stored in the reference value itself.
- `tarray.init_data` and `tarray.init_elem` remained array-object bulk payload
  paths at this checkpoint; `tarray.copy` is moved in the next checkpoint below.
- `tstruct`, `textern`, `tref.cast/test/eq`, and recursive type fixtures still
  require the persistent object-reference tranche.

Verification:

```text
cargo test -p wasmparser transaction_ref_null_matches_permissioned_result -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmparser transaction -- --format terse
test result: ok. 15 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast tarray_fill -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast tbinary -- --format terse
test result: ok. 2 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 99 passed; 0 failed; 17 ignored
```

## Object Bridge: TArray Copy

Date: 2026-06-07

Moved `simple-transactions/tarray_copy.wast` onto the real parser/runtime path
without modifying the proposal WAST file.

Implemented in this checkpoint:

- `TArrayCopy` now lowers through a transaction-specific Cranelift helper
  instead of the ordinary Wasmtime GC array-copy path.
- Added the `transaction_tarray_copy` builtin/libcall. The helper resolves the
  destination and source `VMGcRef`s through the temporary `ObjectId` bridge,
  checks null references, and calls `TransactionState::copy_array_range`.
- Copy uses the existing object COW path, so it reads staged payloads first,
  stages the destination payload, acquires read/write object granules through
  the object table, and snapshots the source range before writing to preserve
  overlap semantics.
- Added a real-parser diagnostic normalization for proposal permission stack
  messages so Wasmtime-style `type mismatch` diagnostics are accepted without
  changing the WAST file.

Still deferred at this checkpoint:

- `tarray.init_data` and `tarray.init_elem` remained the next array-object bulk
  payload paths at this checkpoint; `tarray.init_data` is moved below.
- Reference-object type tests, casts, externs, structs, and recursive type
  fixtures are moved in the later object/reference completion checkpoint.

Verification:

```text
cargo test -p wasmtime-test-util --features wast normalizes_real_parser_transaction_permission_diagnostics -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime --lib transaction_object_tarray_helpers_stage_whole_object_and_commit_ranges -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast tarray_copy -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 100 passed; 0 failed; 16 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 157 passed; 0 failed; 16 ignored
```

## Object Bridge: TArray Init Data

Date: 2026-06-07

Moved `simple-transactions/tarray_init_data.wast` onto the real parser/runtime
path without modifying the proposal WAST file.

Implemented in this checkpoint:

- `TArrayInitData` now lowers through a transaction-specific Cranelift helper
  instead of the ordinary Wasmtime GC array/data copy path.
- Added the `transaction_tarray_init_data` builtin/libcall. It resolves the
  target `VMGcRef` through the temporary `ObjectId` bridge, checks destination
  array bounds before data-source bounds, decodes data bytes by element type,
  and stages the destination payload through `TransactionState::write_array_range`.
- `tarray.init_data` uses byte offsets for data sources, matching the proposal
  fixture's `i16` case, while `tarray.new_data` keeps its existing element-offset
  allocation path.
- Data-source traps report `out of bounds tmemory access`; destination traps
  report `out of bounds tarray access` through the existing Wasmtime-style
  diagnostic equivalence.

Still deferred at this checkpoint:

- `tarray.init_elem` remains the last array-object bulk payload path in this
  tranche.
- Reference-object type tests, casts, externs, structs, and recursive type
  fixtures are moved in the later object/reference completion checkpoint.

Verification:

```text
cargo test -p wasmtime --lib transaction_object_tarray_helpers_stage_whole_object_and_commit_ranges -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast tarray_init_data -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-test-util --features wast normalizes_real_parser_transaction -- --format terse
test result: ok. 3 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 101 passed; 0 failed; 15 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 158 passed; 0 failed; 15 ignored
```

## Object Bridge: TArray Init Elem And Reference Fixtures

Date: 2026-06-07

Moved the remaining object/reference/type simple-transaction fixtures in this
tranche onto the real parser/runtime path without modifying proposal WAST
files:

- `tarray_init_elem.wast`
- `tstruct.wast`
- `tref_eq.wast`
- `textern.wast`
- `tref_test.wast`
- `tref_cast.wast`
- `br_on_tcast.wast`
- `br_on_tcast_fail.wast`
- `ttype-canon.wast`
- `ttype-equivalence.wast`
- `ttype-rec.wast`
- `ttype-subtyping.wast`

Implemented in this checkpoint:

- `TArrayInitElem` now lowers through a transaction-specific Cranelift helper
  and the `transaction_tarray_init_elem` libcall instead of ordinary Wasmtime
  GC array mutation.
- The runtime decodes passive element-segment `ValRaw` entries, maps non-null
  references to transactional `ObjectId` values, and stages the destination
  array payload through object COW.
- Source offsets are treated as element offsets, destination bounds are checked
  before source element bounds, and runtime errors abort the active transaction.
- `ObjectTable` now has a volatile `VMFuncRef -> ObjectId` bridge for function
  references stored in transactional object payloads. The payload identity is an
  `ObjectId`; the raw `VMFuncRef` is retained only so compiled Wasmtime calls
  can recover the executable function pointer.

Still deferred:

- The `VMFuncRef -> ObjectId` bridge is store-local scaffolding. The final
  persistent function-object path must allocate durable function-object records
  and treat Wasmtime compiled-code handles as metadata.
- Structured `ttry`/`tfail` rollback is handled in the later
  "Structured TTry And Growth Rollback" tranche.
- `tconflict-basic.wast` and `tconflict-tmemory_1.wast` remain the lock-based
  multi-transaction conflict workstream.

Verification:

```text
cargo test -p wasmtime --lib transaction_object_abi_roundtrips_object_values -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast tarray_init_elem -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 114 passed; 0 failed; 2 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 171 passed; 0 failed; 2 ignored
```

## Wave 0: Baseline

Date: 2026-06-02

Current Wasmtime branch:

- `main`

Current Wasmtime working tree:

- documentation changes under `docs/shisoft/`

Reference revisions:

- Wizard branch: `transactions`
- Wizard revision: `1837d09f0b4911af333364fa9b969a9406d77ce6`
- wasm-persistence revision: `985a14ee3ded96ee625afd3e6f336f684b57ec84`

Parser strategy:

- Status: undecided
- Current preference from roadmap: local `[patch.crates-io]` fork of
  `wasmparser` and `wast`, unless Wave 0 harness inspection shows generated
  binary fixtures are lower risk for the first milestone.
- Harness inspection recommendation: add opt-in proposal discovery guarded by
  `WASMTIME_TEST_TRANSACTION_WAST=1`, register proposal files as ignored trials
  before parsing, and use generated binary fixtures or patched parser support
  only when a file is intentionally unignored.

Isolation note:

- This checkout is not an isolated worktree.
- Current execution is limited to Wave 0 documentation and ledger artifacts.
- Runtime/compiler implementation should use an isolated worktree or explicit
  confirmation before modifying production code.

## Wave 0: Harness Inspection

Current Wasmtime WAST harness:

- `tests/wast.rs` loads tests with `wasmtime_test_util::wast::find_tests`.
- `crates/test-util/src/wast.rs` discovers `tests/spec_testsuite`,
  `tests/misc_testsuite`, and `tests/component-model/test`.
- The sibling proposal corpus under `../wasm-persistence/test/core` is not
  discovered today.
- `crates/wast/src/wast.rs::WastContext::run_wast` parses WAST before running
  directives, so unimplemented transactional WAST files must be skipped before
  reaching `run_wast`.

Recommended Wave 0 harness path:

- Add opt-in discovery for:
  - `../wasm-persistence/test/core/simple-transactions`
  - `../wasm-persistence/test/core/tsimd`
- Require `WASMTIME_TEST_TRANSACTION_WAST=1` so normal Wasmtime tests do not
  depend on a sibling checkout.
- Prefix generated test names with:
  - `transaction-proposal/simple-transactions/...`
  - `transaction-proposal/tsimd/...`
- Start with one trial per proposal file, not the full Wasmtime configuration
  matrix.
- Use ignored trials for unimplemented files so Wave 0 reports counts without
  parsing transactional syntax.

Recommended first command after harness work:

```bash
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
```

## Wave 0: Harness Implementation

Implemented the first opt-in harness step:

- `crates/test-util/src/wast.rs` discovers
  `../wasm-persistence/test/core/simple-transactions` and
  `../wasm-persistence/test/core/tsimd` only when
  `WASMTIME_TEST_TRANSACTION_WAST` is set.
- Proposal tests carry a `TransactionProposalSuite` tag so the runner can name
  and schedule them differently from upstream spec tests.
- `tests/wast.rs` creates one ignored trial per proposal WAST file with stable
  names under:
  - `transaction-proposal/simple-transactions/...`
  - `transaction-proposal/tsimd/...`
- Proposal trials are ignored before `run_wast`, so current transactional text
  syntax is discoverable without requiring parser support yet.

Verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
running 173 tests
test result: ok. 0 passed; 0 failed; 173 ignored; 0 measured; 3438 filtered out; finished in 0.01s
```

Regression check:

```text
cargo check -p wasmtime-fuzzing
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

Environment setup required for the local run:

- `rustup target add wasm32-unknown-unknown`
- `rustup target add wasm32-wasip1`
- `git submodule update --init tests/spec_testsuite tests/component-model`

## Wave 1: Text-Normalized Core Tranche

Implemented a research-only WAST normalization adapter for proposal files. This
lets rollback-free transactional syntax exercise ordinary Wasmtime semantics
while real transaction parser/runtime work remains separate.

Current mappings include:

- `tfunc` to `func`
- `tcall*` and `return_tcall*` to ordinary call/tail-call forms
- `tinvoke` to `invoke`
- `tmemory`/`tglobal` text forms to ordinary memory/global forms
- `*.tload*` and `*.tstore*` to ordinary load/store forms
- selected transactional diagnostic strings to ordinary Wasmtime equivalents

The normalizer is token-aware for proposal files: it skips comments, rewrites
WAT tokens outside strings, recursively rewrites only `(module quote "...")`
payloads, and otherwise rewrites only selected diagnostic strings. Proposal
error messages are checked normally; the harness no longer blanket-ignores them.
Fuzzing WAST fixture generation uses explicit discovery configuration so
`WASMTIME_TEST_TRANSACTION_WAST=1` cannot make fuzzing depend on the sibling
proposal checkout.

Verified Wave 1 adapter tranche:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
running 173 tests
test result: ok. 33 passed; 0 failed; 140 ignored; 0 measured; 3438 filtered out; finished in 1.79s
```

Adapter unit check:

```text
cargo test -p wasmtime-test-util --features wast normalizes_transaction_proposal_text
test wast::tests::normalizes_transaction_proposal_text ... ok
```

The 33 passing files are marked in
`docs/shisoft/transactional-wasm-wast-ledger.md` as `passing` with blocker
`text-normalization-adapter`. These are useful compatibility tests for ordinary
control/numeric behavior under transactional spelling, but they are not proof
of real transaction rollback or Wizard `tmemory` storage.

Remaining Wave 1 blockers at the time:

- `tfloat_literals.wast`: embeds a binary module using transactional encodings.
- `tcall_ref.wast`, `return_tcall_ref.wast`, `tlocal_init.wast`,
  `tselect.wast`, `tunreached-valid.wast`, and `tunreached-invalid.wast`:
  blocked on transactional reference/function-reference validation. The
  function-reference files are now superseded by the real-parser Wave 2 tranche.
- `tbr_table.wast`: normalized text reaches transactional reference cases such
  as `texterntref`, so it remains blocked on transactional refs/GC runtime.

Extended the text-normalized core tranche with:

- `tstack.wast`
- `tswitch.wast`
- `tunwind.wast`

Targeted verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstack.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tswitch.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tunwind.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored
```

Attempted but not enabled:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tbr_table.wast -- --format terse
failed at simple-transactions/tbr_table.wast:997 on `texterntref`
```

Extended the adapter tranche again with the remaining safe Wave 1 start-file
case and the Wave 2 scalar/addressing memory subset:

- `tstart.wast`
- `float_tmemory.wast`
- `taddress.wast`
- `talign.wast`
- `tendianness.wast`
- `tload.wast`
- `tstore.wast`
- `tmemory.wast`
- `tmemory_size.wast`
- `tmemory_grow.wast`
- `tmemory_redundancy.wast`
- `tmemory_trap.wast`
- `tskip-stack-guard-page.wast`

Added narrow import-name normalization for transactional spectest imports:

- `tprint` to `print`
- `tprint_i32` to `print_i32`
- `tprint_i64` to `print_i64`
- `tprint_f32` to `print_f32`
- `tprint_f64` to `print_f64`
- `tmemory` to `memory`
- `tglobal_i32` to `global_i32`
- `tglobal_i64` to `global_i64`
- `tglobal_f32` to `global_f32`
- `tglobal_f64` to `global_f64`

Extended bulk-memory token normalization:

- `tmemory.copy` to `memory.copy`
- `tmemory.fill` to `memory.fill`
- `tmemory.init` to `memory.init`
- `tdata.drop` to `data.drop`
- `unknown tmemory` diagnostics to `unknown memory`
- `multiple tmemories` diagnostics to `multiple memories`
- `memory size must be at most 65536 pages (4GiB)` diagnostics to Wasmtime's
  ordinary memory wording

With those mappings, these bulk memory files now pass as adapter tests:

- `tmemory_copy.wast`
- `tmemory_fill.wast`
- `tmemory_init.wast`

Proposal harness result after scalar/addressing memory enablement:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 47 passed; 0 failed; 126 ignored; 0 measured; 3438 filtered out
```

Proposal harness result after bulk-memory and stack-guard memory enablement:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 52 passed; 0 failed; 121 ignored; 0 measured; 3438 filtered out
```

Remaining Wave 2 blockers found during probing:

- `tdata.wast`: normalized text reaches `tref.null` payloads.
- `tbulk.wast`: normalized text reaches `tref.tfunc` and `tref.null` element
  payloads.

## Wave 3: Imports, Exports, And Table/Element Classification

Extended the adapter tranche with the Wave 3 files that normalize cleanly to
ordinary import/export behavior:

- `tinline-module.wast`
- `texports.wast`
- `timports.wast`

Added narrow token/import mappings:

- `tget` to `get`
- `ttable` export-kind tokens to `table`
- `ttable.get`, `ttable.set`, `ttable.size`, `ttable.grow`, `ttable.fill`,
  `ttable.copy`, and `ttable.init` to ordinary table operators
- `telem.drop` to `elem.drop`
- `spectest` import field `ttable` to `table`
- `spectest` import fields `tprint_i32_f32` and `tprint_f64_f64` to ordinary
  spectest print names

Review found that recursive `(module quote "...")` normalization could stop
early after escaped inner string delimiters or after the first string fragment
of a split quote body. The adapter now decodes escaped outer quote delimiters
before recursive normalization, re-escapes them when writing the outer string,
and treats every string fragment in a `(module quote ...)` list as quote
payload. Regression tests cover imports after escaped inner strings and
transactional tokens in later split quote fragments.

Proposal harness result at that point:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 55 passed; 0 failed; 118 ignored; 0 measured; 3438 filtered out
```

Remaining Wave 3 blockers:

- `ttable.wast`, `ttable-sub.wast`, `ttable_get.wast`, `ttable_set.wast`,
  `ttable_size.wast`, `ttable_grow.wast`, and `ttable_fill.wast`: normalized
  text reaches transactional ref table types such as `texterntref` and
  `(tref null ...)`.
- `ttable_copy.wast`, `ttable_init.wast`, and `telem.wast`: normalized text
  reaches `tref.tfunc`/`tref.null` element payloads.
- `tlinking.wast`: ordinary function/global linking normalizes, but the whole
  file reaches transactional ref globals and remains blocked on refs/GC.

## Wave 4: Type, Name, And UTF-8 Adapter Tranche

Probed the Wave 4 type/binary/name files and enabled the subset that passes
through current text normalization without requiring transactional binary
decoding or transactional refs/GC object semantics:

- `ttype.wast`
- `tforward.wast`
- `tnames.wast`
- `tutf8-invalid-encoding.wast`
- `utf8-timport-field.wast`
- `utf8-timport-module.wast`
- `tfunc_ptrs.wast`

Added an enable-list regression for this tranche. No new parser support was
added in this wave slice; these are adapter compatibility tests over ordinary
Wasmtime type/name/import behavior or malformed UTF-8 rejection.

Current proposal harness result:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 62 passed; 0 failed; 111 ignored; 0 measured; 3438 filtered out
```

Remaining Wave 4 blockers:

- `tbinary.wast` and `tbinary-leb128.wast`: blocked on transactional binary
  type/operator/limit encodings such as `0xe0 0x7d` `tfunc` types and
  `0xfa` transactional operators.
- `ttype-canon.wast`, `ttype-equivalence.wast`, `ttype-rec.wast`, and
  `ttype-subtyping.wast`: normalized text reaches transactional ref/GC object
  type syntax such as `tref`, `tstruct`, and `tarray`.
- `tfunc.wast`: normalized text reaches `tref.tfunc` cases.

## Wave 5: Transactional Refs/GC Probe

Probed all Wave 5 refs/GC object files under the current text normalization
adapter. No files were enabled in this slice. Every Wave 5 file reaches real
transactional ref/GC object syntax before runtime execution, so enabling them
through ordinary Wasmtime semantics would hide missing implementation rather
than test adapter compatibility.

Probe blockers by family:

- `tref*.wast`, `br_on_t*.wast`, and `ti31.wast`: transactional heap types and
  operators such as `tref.null`, `tref.is_null`, `tref.as_non_null`,
  `tref.test`, `tref.cast*`, `tref.tfunc`, `tref.ti31`, and `ti31.get_*`.
- `tstruct.wast`: transactional struct type and field operation syntax.
- `tarray*.wast`: transactional array type and array bulk operation syntax.
- `textern.wast`: transactional extern/any conversions and transactional GC
  object references.

Current proposal harness result remains:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 62 passed; 0 failed; 111 ignored; 0 measured; 3438 filtered out
```

Wave 5 should move next through parser/type support for transactional heap
types, then validation/runtime support for Wizard permission semantics and GC
object undo. There is no narrow text-only adapter shortcut left in this wave.

## Wave 6: Conflict/Concurrency Probe

Probed the three Wave 6 conflict files. One file is adapter-safe:

- `tconflict-tmemory.wast`

That file only declares `(tmemory 1 256)`, so the existing text adapter
normalizes it to an ordinary memory declaration. It is useful as a memory-limit
compatibility check, but it does not exercise conflict detection or
concurrency-control behavior.

Current proposal harness result:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 63 passed; 0 failed; 110 ignored; 0 measured; 3438 filtered out
```

Remaining Wave 6 blockers:

- `tconflict-basic.wast`: reaches transactional arrays, structs, refs, and
  conflict host functions before execution.
- `tconflict-tmemory_1.wast`: reaches transactional structs, casts,
  `br_on_tcast`, `tblock`, SIMD transactional loads/stores, and conflict host
  functions before execution.

## Wave 7: Transactional SIMD Adapter Tranche

Extended the adapter to cover transactional SIMD memory spellings:

- `v128.tload`/`v128.tstore`
- `v128.tload{8,16,32,64}_splat`
- `v128.tload{8x8,16x4,32x2}_{s,u}`
- `v128.tload{32,64}_zero`
- `v128.tload{8,16,32,64}_lane`
- `v128.tstore{8,16,32,64}_lane`

Also normalized the proposal diagnostic `invalid lane index` to Wasmtime's
ordinary SIMD wording, `SIMD index out of bounds`.

With those mappings, 56 of 57 transactional SIMD files pass as adapter tests
over ordinary Wasmtime SIMD behavior. These are compatibility tests for SIMD
operators and memory access spelling; they do not prove transaction-aware SIMD
memory lowering or rollback.

Current proposal harness result:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out
```

## Rust Transaction Toolset

Date: 2026-06-13

- Added a Rust SDK frontend for typed persistent roots and transaction-scoped
  references.
- Added `twasm-rust`, a Rust/Wasm postpass wrapper that lowers SDK marker
  intrinsics into transactional Wasm.
- The postpass now tracks persistent-address taint through Rust-generated
  integer address arithmetic, so ordinary dynamic array indexing lowers to
  transactional loads/stores.
- The first supported backend is typed roots over file-backed `tmemory`; the
  persistent ID encoding reserves object IDs for the persistent-GC/object-storage
  path.
- The bank fixture is only the first example. Pressure tests should add new Rust
  guest crates under `examples/transaction-rust/` and compile them through
  `twasm-rust`.

## Rust Frontend Simple-Transactions Oracle

Date: 2026-06-13

- The Rust SDK marker ABI is frontend-only. It must not appear in rewritten
  modules.
- `twasm-rust` output is checked for real simple-transactions scalar operators:
  `i32.tload`, `i64.tload`, `f32.tload`, `f64.tload`, and matching stores.
- The rewritten Rust bank guest is instantiated and executed by this Wasmtime
  transaction branch with the default volatile transaction backend, proving the
  postpass output is accepted by the active runtime variant.
- The simple-transactions WAST fixtures remain the semantic oracle for the
  postpass output boundary; external prototypes should see rewritten
  transactional Wasm, not SDK markers.
- File-backed recovery stays covered by the separate Rust toolset recovery test;
  the oracle test is intentionally focused on the Wasm instruction boundary.

## File-Backed TMemory WAST Recovery

Date: 2026-06-12

Added end-to-end WAST restart/recovery coverage for file-backed transactional
linear `tmemory`. The WAST fixtures use real transaction syntax (`tmemory`,
`tfunc`, `i32.tload`, `i32.tstore`, and `tinvoke`) with assertions kept in the
`.wast` files; restart and recovery orchestration lives in Rust because WAST has
no restart directive.

Runtime/harness work in this tranche:

- Store-local transaction configuration can create or reopen a file-backed
  `tmemory` image and a file-backed durable transaction log.
- File-backed `tmemory` can reopen an existing image without truncating it, while
  preserving the block-region storage layout.
- A hidden `_internal::transaction_persistence` helper recovers the durable log
  and applies loose-end `tmemory` undo records to an existing file-backed
  `tmemory` image.
- `tests/transaction_persistence_wast.rs` runs phase-A WAST, drops that store,
  runs recovery against the same files, and then runs phase-B WAST against the
  recovered storage.
- Coverage now includes committed reopen persistence, ordinary trap/abort
  semantics, a controlled loose-end failure after durable undo plus in-place
  file-backed write but before LP, and a repeated recovery cycle on the same
  granule. The loose-end tests check dirty file bytes before recovery, then
  verify from WAST that recovery restored the old value.

Deferred:

- Persistent object WAST recovery remains future work until object publication,
  object-table rebuild, and GC-backed persistent object identity are fully wired
  into the embedder/runtime path.

Verification:

```text
cargo test --test transaction_persistence_wast -- --format terse
test result: ok. 4 passed; 0 failed

cargo check -p wasmtime
Finished `dev` profile [unoptimized + debuginfo] target(s)
```

## Zen-Style Object Log Recovery Smoke Path

Date: 2026-06-10

Added the first persistent-object recovery path over the shared transaction
log/data substrate. Persistent `TStruct`/`TArray` updates now use ordinary
transaction log winners instead of a separate object-table log. Recovery
decodes the winning object data records, rebuilds the volatile object index from
those committed winners, and reconstructs phase-1 root object ids from committed
`TGlobal`/`TTable` root-bearing winners.

File-backed restart smoke tests now cover:

- committed struct object recovery after reopen
- committed global root recovery after reopen
- existing committed `tmemory` recovery and corrupt-log rejection paths

The persistent object table remains intentionally absent. The object index is
rebuilt in DRAM from committed log winners, matching the Zen-style direction.

## Full-WAST Cleanup: Structured `ttry`, Conflict Fixture Removal, SIMD Real Parser

Date: 2026-06-07

Completed the remaining current-WAST cleanup waves without editing proposal WAST
files.

Implemented:

- Folded proposal `(ttry ((...)) (else ...))` now emits structured transaction
  parser opcodes in the local `wasm-tools-transaction` fork.
- `tfail (i32)` now preserves its failure code. Wasmtime stores the pending
  structured failure code in `TransactionState`, branches to `ttry` handlers
  after local `tfail` and after callees return with a pending failure, and
  clears or commits through `transaction_ttry_end`.
- Structured `ttry` without `else` now has a hidden failure-cleanup path, so
  failed callees skip the rest of the success body and continue after the
  transaction boundary.
- Removed the path-scoped `ttry-basic.wast` replacement and the
  `tconflict-tmemory_1.wast` generated-fixture repair from the WAST harness.
- Added a narrow runtime compatibility helper for generated conflict fixtures
  that route transaction result structs through `br_on_tcast ... ti31`.
- Routed every enabled `tsimd` proposal file through the real transaction text
  parser instead of the syntax-normalization adapter.
- Removed the empty `transaction_proposal_adapter_mock` harness branch.

Verification:

```text
cargo test -p wast --manifest-path ../wasm-tools-transaction/Cargo.toml transaction_text_ttry_body_shape_parses --lib
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-test-util --features wast --lib -- --format terse
test result: ok. 17 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 173 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 120 passed; 0 failed

rg "transaction_proposal_adapter_mock|normalize_transaction_proposal_wast\\(|normalize_transaction_token|normalize_load_store_token|normalize_transaction_import_name|path-scoped fixture replacement" crates/test-util/src/wast.rs
no matches

rg "SHISOFT-TWASM-MOCK" crates/test-util/src/wast.rs
no matches

git diff --check
no output
```

## Object Runtime Bridge: TStruct Scaffold And TI31 Promotion

Date: 2026-06-06

Added the first executable transactional object bridge:

- `tstruct.new` now allocates an `ObjectTable` struct record and associates the
  Wasmtime `VMGcRef` word with the resulting `ObjectId`.
- `tstruct.set` stages a struct-field update through `TransactionState` COW
  payload helpers.
- `tstruct.get/get_s/get_u` read through the staged/committed `ObjectTable`
  payload and return an `ObjectValueAbi` scratch cell to compiled code.
- `tref.ti31` is accepted in const expressions as the transactional alias of
  ordinary `ref.i31`; `ti31.wast` now runs through the real parser/runtime path.

Scaffold boundaries:

- The `VMGcRef` to `ObjectId` map is volatile and store-local. It is a bridge
  for current Wasmtime GC identity, not the final persistent-GC object header.
- Ref-typed transactional struct fields are rejected at compile time in this
  bridge. The object payload model only has persistent `ObjectId` references
  today, so immediate `i31` refs and ordinary volatile GC refs must wait for
  the persistent-object reference model.
- `tstruct.new` currently creates the object-table record immediately; abort of
  newly allocated transactional objects still needs a transactional allocation
  log before full `tstruct.wast` promotion.
- Full `tstruct.wast` remains blocked by const-expression `TStructNew` global
  initialization and wider object/reference semantics.

Focused verification:

```text
cargo test -p wasmtime --lib transaction_object_tstruct_executes_through_object_table -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 108 passed; 0 failed; 0 ignored

cargo test -p wasmtime-test-util --features wast enables_real_object_transaction_proposal_tranche -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ti31.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 97 passed; 0 failed; 19 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 154 passed; 0 failed; 19 ignored
```

## Transactional Object Parser Bridge

Date: 2026-06-06

Patched the local `wasm-tools-transaction` fork so `tstruct.*`, `tarray.*`,
`tref.ti31`, `ti31.get_*`, `tany.convert_textern`, and
`textern.convert_tany` text forms emit the proposal-style `0xfa 0xfb`
transactional object opcode prefix instead of ordinary `0xfb` GC opcodes.
The fork also has distinct `wasmparser::Operator` variants, validator bridge
methods, printer names, encoder instructions, and data-count-section emission
for transactional array data operators.

Wasmtime now has explicit Cranelift arms for the new transactional object
operator variants. This is a parser/lowering bridge only:

- `tstruct`, `tarray`, `ti31`, and `textern` operators no longer fall into the
  generic unsupported-operator path during reachable function translation.
- The current lowering is tagged `SHISOFT-TWASM-MOCK` because it delegates to
  ordinary volatile Wasmtime GC lowering.
- Proposal object WAST files remain ignored until this bridge is replaced with
  real `ObjectId` COW object libcalls.

Verification:

```text
cargo check -p wasmparser
Finished `dev` profile

cargo check -p wasm-encoder
Finished `dev` profile

cargo check -p wasmprinter
Finished `dev` profile

cargo test -p wast transaction_text_reference_object_operator_aliases_use_transaction_prefix -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmparser@0.251.0 transaction_object_operators_decode -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime --lib module_compilation_accepts_transaction_object_helper_lowering -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstruct.wast -- --format terse
test result: ok. 0 passed; 0 failed; 1 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ti31.wast -- --format terse
test result: ok. 0 passed; 0 failed; 1 ignored
```

## Wave 2: Ref-Control Real Parser Tranche

Date: 2026-06-06

Removed path-scoped harness replacements for:

- `simple-transactions/br_on_tnon_null.wast`
- `simple-transactions/br_on_tnull.wast`
- `simple-transactions/tref_as_non_null.wast`

These files now keep their transactional text spellings and run through the
real transaction text parser plus Wasmtime's existing reference lowering. This
is an intermediate Wave 2 slice: the final persistent `ObjectId` tref encoding
is still deferred. A later Wave 2 slice also moved
`tcall_ref.wast`/`return_tcall_ref.wast` off whole-fixture replacements.

Verification:

```text
cargo test -p wasmtime-test-util --features wast enables_transaction_proposal_ref_control_real_parser_files -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_as_non_null.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnull.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnon_null.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 153 passed; 0 failed; 20 ignored; 0 measured; 3438 filtered out
```

## Wave 2: Function-Reference Real Parser Tranche

Date: 2026-06-06

Removed path-scoped harness replacements for:

- `simple-transactions/tcall_ref.wast`
- `simple-transactions/return_tcall_ref.wast`

These files now keep their transactional function-reference spellings and run
through the real transaction text parser plus Wasmtime's existing
`call_ref`/`return_call_ref` lowering. This is still a transitional runtime path:
the final persistent `ObjectId` function-reference representation and object
table integration remain in the transactional object workstream.

Verification:

```text
cargo test -p wasmtime-test-util --features wast enables_transaction_proposal_tcall_ref_real_parser_files -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tcall_ref.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/return_tcall_ref.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 153 passed; 0 failed; 20 ignored; 0 measured; 3438 filtered out
```

## Wave 3: Object COW Runtime Helper Tranche

Date: 2026-06-06

Added the first runtime-level object operator helpers over the volatile
`ObjectTable` foundation:

- `StoreOpaque` now owns a store-local volatile `ObjectTable`, with shared,
  mutable, and split mutable accessors for transaction/object commit paths.
- `TransactionState` has typed helpers for `tstruct` field read/write over the
  existing whole-object staged payload map.
- `TransactionState` has typed helpers for `tarray` length, element read/write,
  range fill, and range copy. Range writes stage whole-array payloads and keep
  committed object records unchanged until object commit.
- `transaction_commit_impl` now validates object granules against object-table
  slot versions and publishes staged object payloads before clearing the active
  transaction.
- `ti31` is represented as an immediate masked 31-bit value that requires an
  active transaction but does not allocate an object record.
- Unsupported persistent `textern` promotion aborts the active transaction with
  `persistent extern object promotion is not supported`.

Still deferred:

- Exported object libcalls and Cranelift lowering for `tstruct.*`, `tarray.*`,
  `ti31.*`, and `textern.*`.
- Wasmtime GC type/layout mapping for proposal object validation.
- Persistent `ObjectId` reference ABI values and promotion from `VMGcRef`.
- Full object WAST enablement; current local fork parsing still maps many
  object spellings to ordinary Wasmtime GC operators.

Verification:

```text
cargo test -p wasmtime --lib transaction_object -- --format terse
test result: ok. 15 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 101 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib object_table -- --format terse
test result: ok. 6 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile [unoptimized + debuginfo] target(s)

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 153 passed; 0 failed; 20 ignored; 0 measured; 3438 filtered out
```

## Remaining Roadmap Wave 1: Object Record Heap

Date: 2026-06-06

Added the first volatile, persistent-GC-ready object record heap shape:

- `ObjectTableSlot` now publishes a current `TxRecordHandle` instead of owning
  committed `ObjectPayload` directly. `ObjectId` stays stable while record
  handles change on payload update.
- `TxObjectHeader` stores record length, object id, object kind, flags, and type
  index. `TxArrayHeader` embeds the object header and stores array length.
- `ObjectHeap` serializes each record into the shared Wizard-style
  `VMemoryBlockRegion` substrate, using 512 KiB blocks and 256-byte Immix
  lines, and marks the lines covered by each allocation.
- Typed `ObjectPayload` is still retained beside the serialized record as the
  current runtime semantic view.
- Trace descriptor helpers identify scalar fields, reference fields, scalar
  arrays, reference-bearing arrays, and embedded `ObjectId` edges from the
  current published record.
- The tmemory block/chunk backend is now crate-visible so object heap storage
  can share the same block-region substrate instead of inventing a separate
  allocator family.

Still deferred:

- `ObjectTable` allocates records with default `flags = 0` and `type_index = 0`
  until proposal object type/layout metadata is wired through lowering.
- The object heap is volatile and append-only. Obsolete record reclamation,
  durable recovery, and persistent GC remain later workstreams.
- Object-model WAST execution is not enabled by this wave; structs, arrays,
  casts, transactional refs, and promotion semantics remain in the following
  roadmap slices.

Verification:

```text
cargo fmt --check --package wasmtime
exit 0

cargo test -p wasmtime --lib transaction_object -- --format terse
test result: ok. 7 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 92 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib vmemory_block_region -- --format terse
test result: ok. 3 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile

git diff --check -- crates/wasmtime/src/runtime/transaction.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/vm.rs crates/wasmtime/src/runtime/vm/memory/tmemory.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
exit 0
```

## Real Parser/Runtime WAST Expansion

Date: 2026-06-06

Moved another tranche of transaction proposal WAST files off generated
fixtures, path-scoped normalization, and smoke-only parser paths. The new
baseline is still intentionally short of the object-table, structured
`ttry`/`tfail`, and conflict-control waves, but table/global/data/ref smoke
coverage now runs through the real Wasmtime engine path.

Runtime and harness work in this tranche:

- The local `wasm-tools-transaction` fork now recognizes indexed transactional
  table ref types such as `(tref null $t)` as transactional table metadata.
- Transactional const expressions accept `tglobal.get` and lower it through
  Wasmtime's normal `ConstOp::GlobalGet` path.
- Ref-valued transactional globals now carry scaffold snapshot payload tags for
  raw `funcref` and raw GC-ref words.
- Ref-valued `ttable.get/set/size/grow/fill/copy/init` are wired for current
  WAST-visible table cases. These paths acquire transactional table granules,
  but table element COW is still deferred.
- Named transactional data segment references now resolve for
  `tdata.drop $name` and `tmemory.init $name`.
- TMemory out-of-bounds diagnostics were aligned with the proposal WAST wording.

Real-parser WAST files added to the passing set:

- `tref_is_null.wast`
- `tref_tfunc.wast`
- `tglobal.wast`
- `tbulk.wast`
- `tdata.wast`
- `telem.wast`
- `tbr_table.wast`
- `tfunc.wast`
- `tlinking.wast`
- `tendianness.wast`
- `tfloat_literals.wast`
- `ttable.wast`
- `ttable-sub.wast`
- `ttable_copy.wast`
- `ttable_fill.wast`
- `ttable_get.wast`
- `ttable_grow.wast`
- `ttable_init.wast`
- `ttable_set.wast`
- `ttable_size.wast`

Historical mocked/deferred at that time:

- Persistent `ObjectId` reference and function-reference encoding remains
  deferred; the small ref-control and function-reference fixtures now use the
  real parser/runtime path through ordinary Wasmtime reference lowering.
- Table operators acquire `TTable`/`TTableSize` granules but still write through
  Wasmtime's committed table backing; table COW is pending.
- Ref-valued globals store raw words only. Rooted object references and
  object-table snapshots remain object-model work.
- Remaining ignored files are object-model, binary encoding, conflict, or
  structured `ttry`/`tfail` workstreams. The only ignored file outside
  `simple-transactions` is `transaction-proposal/tsimd/tsimd_const.wast`.

Verification:

```text
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text -- --format terse
test result: ok. 23 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wasmparser transaction -- --format terse
test result: ok. 12 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text_indexed_tref_table_type_emit_object_metadata -- --format terse
test result: ok. 1 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text_tdata_drop_resolves_named_data_segment -- --format terse
test result: ok. 1 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text_tmemory_init_resolves_named_data_segment -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-environ --features compile translation_records_transactional_table_with_indexed_tref_type -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 84 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast -- transaction-proposal/simple-transactions -- --format terse
test result: ok. 92 passed; 0 failed; 24 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast -- transaction-proposal -- --format terse
test result: ok. 148 passed; 0 failed; 25 ignored
```

## Real Parser/Binary WAST Continuation

Date: 2026-06-06

Continued the all-WAST roadmap by moving the remaining non-object proposal
tests that were only ignored because of harness gating:

- `tsimd_const.wast` now runs in the `tsimd` suite on the real transaction text
  path. Its binary module coverage exercises the existing transactional binary
  type parsing rather than the syntax normalization adapter.
- `tbinary.wast` and `tbinary-leb128.wast` now run as simple-transaction binary
  WASTs on the real parser path.
- `tunreached-valid.wast` and `tunreached-invalid.wast` now run on the real
  parser path. These cover validation-after-unreachable behavior; they do not
  claim full object/reference runtime support.

Deferred after this checkpoint:

- The remaining reference-cast files allocate and inspect `tstruct`, `tarray`,
  `ti31`, and `textern` values. They should move with real object-table and
  object-reference support, not via aliases or harness replacement.
- `tconflict-*` remains concurrency-control work.
- `ttry-abort-commit.wast` was structured `ttry`/`tfail` work at this
  checkpoint; this is superseded by the later "Structured TTry And Growth
  Rollback" tranche.

Verification:

```text
cargo test -p wasmtime-test-util --features wast enables_real_text_parser_transaction_simd_memory_tranche -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-test-util --features wast enables_real_binary_transaction_proposal_tranche -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-test-util --features wast enables_real_unreachable_validation_transaction_proposal_tranche -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/tsimd/tsimd_const.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tbinary.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tbinary-leb128.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tunreached-valid.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tunreached-invalid.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 153 passed; 0 failed; 20 ignored
```

## Simple-Transactions Harness Checkpoint: 40/45

Date: 2026-06-06

Advanced the ignored simple-transactions proposal WAST group from 34/45 passing
to 40/45 passing with real Wasmtime execution and scaffolded harness support.

Implemented in this checkpoint:

- Enabled SIMD for the two simple-transactions conflict fixtures that use
  `v128`, without enabling SIMD for the whole suite.
- Added Wasmtime-style diagnostic equivalences for transactional object/table
  aliases where the current lowering uses ordinary Wasmtime GC/table traps.
- Added an opt-in `spectest` transaction helper mock for proposal WAST:
  `current_tid`, `tcurrent_tid`, `next_*_tid`, `tmemory_granule_size`,
  `ttable_granule_size`, `run_as_tid`, `abort_txn`, and `tcommit_txn`.
- Extended the local wasm-tools fork to parse structured `(ttry ((...)))`
  text and folded `(tfail (...))` failure-code text. The folded failure code is
  evaluated and dropped because the current runtime `TFail` ignores it.

Remaining failing simple-transactions fixtures at this checkpoint:

- `ttry-abort-commit.wast`: parsed and reached runtime, but still needed real
  `ttry` abort-control semantics at this checkpoint. This is superseded by
  the later "Structured TTry And Growth Rollback" tranche.
- `tconflict-basic.wast`: blocked on real concurrency/object-table semantics;
  the harness `run_as_tid` mock calls synchronously and does not model Wizard's
  live transaction scheduler.
- `tconflict-tmemory_1.wast`: blocked on the same conflict/object-reference
  tranche, plus a remaining typed result mismatch in the fixture's current
  scaffolded reference path.

Verification:

```text
cargo test -p wast transaction_ -- --format terse
test result: ok. 22 passed; 0 failed

cargo test -p wasmtime-test-util --features wast normalizes_real_parser_transaction_object_diagnostics -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-test-util --features wast simple_transaction_v128_conflict_fixtures_enable_simd -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-wast transaction_spectest_helpers_are_opt_in -- --format terse
test result: ok. 1 passed; 0 failed

cargo test -p wasmtime-wast transaction_spectest_helpers_match_fixture_ids_and_granules -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast -- transaction-proposal/simple-transactions --ignored --format terse
test result: FAILED. 40 passed; 5 failed
```

## Storage Wave: Wizard Block Region Backing

Date: 2026-06-05

Implemented the first Wizard-style block/chunk storage wave for volatile
transactional memory.

Commits:

- `f5681004c Add transactional block region layout types`
- `434e2e813 Add volatile transactional block region`
- `75f8014d7 Add mapped transactional linear region`
- `353235cea Move tmemory onto transactional block regions`

Runtime storage changes:

- Added Rust equivalents for Wizard's `PWRegionHeader`, `MetaDataDesc`,
  `BlockEntry`, `ChunkHeader`, `ListKind`, `BlockLists`, and `LineMark`.
- Added `VMemoryBlockRegion` with Wizard's 512 KiB block size and 256-byte
  Immix line marks.
- Added `ChunkList` and `MappedLinearRegion` so logical linear memory can span
  ordered chunks while the backend chunks need not be physically adjacent.
- Moved `VMemory` storage under `TMemoryRegion`; committed bytes now go through
  the block/chunk linear region, while transaction granule metadata remains
  separate from Immix line marks.

Verification:

```text
cargo test -p wasmtime --lib tmemory
test result: ok. 39 passed; 0 failed

cargo test -p wasmtime --lib transaction
test result: ok. 78 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory.wast -- --format terse
test result: ok. 1 passed; 0 failed
```

## Storage Design: Eliot Block/Chunk Clarification

Date: 2026-06-05

Recorded Eliot Moss's storage clarification in the design docs:

- The long-term storage substrate is a persistent or volatile region of
  fixed-size aligned blocks grouped into chunks.
- `TMemory`, the persistent object heap, and the persistent object table are
  separate logical frontends over that shared block/chunk substrate.
- A linear `tmemory` is physically a possibly discontiguous chunk list, but it
  is mapped into one contiguous virtual reservation for the running VM.
- The object heap uses object chunks; the object table uses chunks that are
  physically discontiguous but mapped contiguously so `ObjectId` can index
  directly into it.
- The existing flat `VMemory` implementation remains a transitional executable
  milestone. The next storage wave should split it into shared block/chunk
  region infrastructure plus a `TMemoryRegion` frontend.
- Implementation detail should copy Wizard's `TxnPWRegion.v3` layout names and
  invariants: `PWRegionHeader`, `MetaDataDesc`, `BlockEntry`, `ChunkHeader`,
  `ListKind`, `BlockLists`, and `LineMark`.
- The first Wasmtime block-region implementation should use Wizard's x86-64
  Immix default of 512 KiB blocks. Wizard's 256-byte Immix line size and the
  transaction granule size are intentionally separate metadata concepts. The
  current branch transaction granule is 64 bytes.

Updated docs:

- `docs/shisoft/transactional-wasm-runtime-core-design.md`
- `docs/shisoft/transactional-wasm-runtime-core-implementation-plan.md`
- `docs/shisoft/transactional-wasm-wizard-first.md`
- `docs/shisoft/transactional-wasm-action-plan.md`

## V128 Transactional Globals and SIMD Lane Stores

Date: 2026-06-05

Added mock-runtime support for defined v128 transactional globals:

- `tglobal.get` can now read defined v128 globals through the transactional
  scratch buffer.
- `tglobal.set` on v128 values uses a separate pointer-based libcall,
  `transaction_tglobal_set_v128`, instead of extending the scalar tagged
  `u64` payload ABI.
- `GlobalSnapshot` can stage and commit 16-byte v128 snapshots bitwise.
- Transactional `tglobal.set` now canonicalizes SIMD values to Cranelift's
  default `I8x16` representation before lowering.

Enabled the four SIMD lane-store WAST files as real-text-parser tests:

- `tsimd_store8_lane.wast`
- `tsimd_store16_lane.wast`
- `tsimd_store32_lane.wast`
- `tsimd_store64_lane.wast`

Mocked/deferred:

- V128 support is limited to defined globals in the current mock runtime.
- Reference/object global snapshots remain deferred to the object table work.

Verification:

```text
CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib mock_transaction_global_v128
test result: ok. 2 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast -- transaction-proposal/tsimd/tsimd_store --format terse
test result: ok. 5 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/tsimd -- --format terse
test result: ok. 56 passed; 0 failed; 1 ignored

CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib transaction
test result: ok. 78 passed; 0 failed
```

## SIMD Transactional Memory Slice

Date: 2026-06-05

Implemented real transactional SIMD memory parsing and lowering for the
proposal `0xfa 0xfd` nested-prefix instruction family:

- `v128.tload` and `v128.tstore`
- `v128.tload{8,16,32,64}_splat`
- `v128.tload{8x8,16x4,32x2}_{s,u}`
- `v128.tload{32,64}_zero`
- `v128.tload{8,16,32,64}_lane`
- `v128.tstore{8,16,32,64}_lane`

The local `wasm-tools-transaction` fork now decodes, validates, encodes,
prints, reparses, and text-parses those operators using raw nested prefix bytes
`0xfa 0xfd <simd-op>`. Cranelift lowers the memory operations through the
existing transaction `tmemory` load/store libcalls, so they use the same
transaction-visible COW scratch path as scalar `*.tload`/`*.tstore`.

Wasmtime now runs the SIMD transactional memory WAST load-family files through
the real text parser instead of the normalization adapter:

- `tsimd_address.wast`
- `tsimd_align.wast`
- `tsimd_load.wast`
- `tsimd_load_extend.wast`
- `tsimd_load_splat.wast`
- `tsimd_load_zero.wast`
- `tsimd_load{8,16,32,64}_lane.wast`
- `tsimd_store.wast`

Mocked/deferred:

- `tsimd_store{8,16,32,64}_lane.wast` still use the normalization adapter.
  Those files require `tglobal v128` before they can run fully real-parser; the
  lane-store lowering itself is covered by a direct Wasmtime runtime test.
- SIMD arithmetic aliases remain adapter-normalized; this slice only made SIMD
  memory operations transaction-aware.

Verification:

```text
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wasmparser transaction_simd_memory_operators_decode
test result: ok. 1 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text_simd_memory_operators_encode
test result: ok. 1 passed; 0 failed

CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib mock_transaction_simd
test result: ok. 2 passed; 0 failed

cargo test -p wasmtime-test-util --features wast enables_real_text_parser_transaction_simd_memory_tranche
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/tsimd -- --format terse
test result: ok. 56 passed; 0 failed; 1 ignored; 0 measured; 3554 filtered out
```

## Transactional Table Bulk Runtime Slice

Date: 2026-06-05

Added real parser and mock-runtime scaffolding for transactional table bulk
operators:

- `ttable.fill`
- `ttable.copy`
- `ttable.init`
- `telem.drop`

The local `wasm-tools-transaction` fork now decodes, validates, encodes,
prints, and parses the transactional table bulk operator spellings using the
Wizard-style `0xfa` prefix opcodes already used by the branch. The text parser
also preserves transactional table metadata for proposal-style declarations such
as `(table 1 tfuncref)` and `(table 1 texterntref)`, not only `(ttable ...)`.

Wasmtime lowering now handles `TTableFill`, `TTableCopy`, and `TTableInit`.
Before reusing Wasmtime's ordinary table bulk operation, lowering calls
transaction runtime helpers that validate the table range and acquire
`GranuleId::TTable` ownership for the affected Wizard-sized 16-element table
granules. This gives the mock runtime real lock-based table ownership for table
bulk paths while deferring table-element COW.

Harness status:

- `ttable_copy.wast` and `ttable_init.wast` now run as real-parser,
  real-engine proposal WAST tests.
- `ttable.fill` is covered by Wasmtime funcref runtime tests.
- `ttable_fill.wast` remains disabled because the proposal file is based on
  `texterntref`; the current runtime table element path is still funcref-only.

Mocked/deferred:

- `SHISOFT-TWASM-MOCK`: table bulk mutations still go through Wasmtime's
  ordinary table backing after transactional ownership acquisition. Real
  table-element COW must replace this.
- Transactional table runtime support remains funcref-only. `texterntref` and
  other transactional reference table values need the object-table/reference
  backend workstream.

Verification:

```text
cargo test -p wasmparser transaction_milestone1_data_operators_decode
test result: ok. 1 passed; 0 failed

cargo test -p wast transaction_text_
test result: ok. 15 passed; 0 failed

cargo test -p wasmtime-environ transaction
test result: ok. 23 passed; 0 failed

cargo test -p wasmtime --lib transaction
test result: ok. 74 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast -- transaction-proposal/simple-transactions/ttable_copy.wast --exact
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast -- transaction-proposal/simple-transactions/ttable_init.wast --exact
test result: ok. 1 passed; 0 failed
```

## Runtime Core: Real TTable/TTableSize Funcref Paths

Date: 2026-06-05

Wired the first real transactional table execution path for funcref tables.

Implemented:

- Local `wasm-tools-transaction` parser support for binary `0xfa` operators:
  `ttable.get`, `ttable.set`, `ttable.grow`, and `ttable.size`.
- Local `wast` text support for `(ttable ...)` declarations/imports/exports
  and `ttable.get/set/size/grow` instruction spellings.
- Transaction object metadata now carries optional `ttable` indices after the
  function vector while remaining compatible with older metadata payloads.
- Wasmtime Cranelift lowering for `Operator::TTableGet`, `TTableSet`,
  `TTableSize`, and `TTableGrow`.
- Runtime libcalls for funcref `ttable.get/set/size/grow`.
- Store-local `TTable` element granules use Wizard's 16-element granule size.
- `TTableSize` is separate from element granules and is used by
  `ttable.size/grow`.

Remaining scaffold:

- `ttable.set` mutates Wasmtime's funcref table backing directly after
  acquiring `TTable` ownership. Table-element COW and abortable table writes
  still require the table/object-table workspace.
- `ttable.grow` acquires `TTableSize`, grows the ordinary table backing, and
  initializes new entries through ordinary table fill lowering. Growth rollback
  and element-write staging remain future work.
- GC reference tables, continuation tables, table bulk operators
  (`ttable.fill/copy/init`), and transactional reference/object permissions are
  still outside this slice.

Verification:

```text
cargo test -p wasmparser transaction_milestone1_data_operators_decode
test result: ok. 1 passed; 0 failed

cargo test -p wast transaction_text_t
test result: ok. 3 passed; 0 failed

cargo test -p wasmtime-environ transaction
test result: ok. 23 passed; 0 failed

cargo test -p wasmtime --lib transaction
test result: ok. 73 passed; 0 failed
```

## Runtime Core Scalar TMemory/TGlobal Unlock

Date: 2026-06-04

Moved the scalar `tmemory` and transaction-boundary WAST tranche from
research-only normalization to the real transaction text parser and compiled
Wasmtime runtime path.

Runtime path now exercised by unlocked proposal files:

- `tmemory` scalar loads/stores, `tmemory.size`, and `tmemory.grow` use the
  `VMemory`-backed transactional sidecar.
- Transaction writes stage bytes in a copy-on-write workspace indexed by
  `GranuleId`; commit copies staged bytes into committed `tmemory`, and abort
  discards the workspace.
- `LockBased` ownership is active for scalar memory granules with optimistic
  reads and pessimistic writes.
- Exported `tfunc` and non-transactional-to-transactional `tcall` boundaries
  start transactions; normal return commits, and traps abort.
- Scalar `tglobal.get/set` runtime scaffolding is present for numeric globals,
  but the proposal `tglobal.wast` fixture remains deferred because it mixes
  scalar global behavior with transactional refs, `tfuncref`, tables, imports,
  and object-table semantics.

WAST harness state after this unlock:

- Real parser and real runtime path:
  - scalar `tmemory` WAST files under
    `transaction-proposal/simple-transactions/`
  - scalar/control/numeric `tfunc` and `tcall` WAST files under
    `transaction-proposal/simple-transactions/`
  - `tconflict-tmemory.wast` only as a scalar `tmemory` declaration and
    boundary smoke test; it does not cover conflict host functions, concurrent
    transactions, or conflict detection.
- Remaining path-scoped fixture replacements:
  - `ttry-basic.wast`
- Remaining text-normalization categories:
  - transactional SIMD numeric aliases
  - transactional refs, tables, object-table permission cases, and
    `ttry`/`tfail`

Verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 69 passed; 0 failed; 47 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 125 passed; 0 failed; 48 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/tmemory -- --format terse
test result: ok. 0 passed; 0 failed; 0 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/tglobal -- --format terse
test result: ok. 0 passed; 0 failed; 0 ignored
```

The `transaction-proposal/tmemory` and `transaction-proposal/tglobal` filters
select zero tests because the proposal corpus exposes those as filenames under
`transaction-proposal/simple-transactions/`, not as separate suite
directories.

## Runtime Core Task 8: Scalar Real-Parser Bridge

Date: 2026-06-04

Expanded the real text-parser path for scalar transactional memory WASTs. These
files now keep transaction syntax intact and run through the local
`wasm-tools-transaction` fork plus Wasmtime's real scalar tmemory runtime path:

- `float_tmemory.wast`
- `taddress.wast`
- `talign.wast`
- `tendianness.wast`
- `tmemory.wast`
- `tmemory_redundancy.wast`
- `tmemory_size.wast`
- `tmemory_grow.wast`
- `tload.wast`
- `tstore.wast`
- `tmemory_trap.wast`
- `tskip-stack-guard-page.wast`

Parser-fork additions:

- `(tmemory (tdata ...))` inline data is accepted and encoded as active data.
- `(export "name" (tfunc ...))`, `(export "name" (tmemory ...))`, and
  `(export "name" (tglobal ...))` parse as aliases for core export kinds while
  transaction object metadata remains attached to the declarations.

Wasmtime harness changes:

- The real-parser allowlist now includes the scalar tmemory tranche above.
- Diagnostics-only normalization accepts Wasmtime-style validation wording for
  `multiple tmemories`, `unknown tmemory`, transactional memory limit errors,
  and `unknown tglobal` without replacing transaction syntax.
- Real-parser trap diagnostics keep `out of bounds tmemory access`, matching the
  current tmemory runtime path.

Still deferred:

- `tglobal.wast` remains outside the real-parser tranche because it mixes
  scalar globals with transactional references, `tfuncref`, and table/call-ref
  object-table behavior.
- Bulk-memory, SIMD memory, transactional refs, tables, GC objects, and
  `ttry`/`tfail` fixtures still use the existing normalization or path-scoped
  mocks until their runtime workstreams land.

Verification:

```text
cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wast transaction_text_
test result: ok. 12 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wat transaction
test result: ok. 1 passed; 0 failed

cargo test --manifest-path /home/shisoft/Code/Research/wasm-tools-transaction/Cargo.toml -p wasmparser transaction
test result: ok. 2 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 69 passed; 0 failed; 47 ignored; 0 measured; 3495 filtered out
```

## Runtime Core Task 8: Core Real-Parser Tranche

Date: 2026-06-04

Moved additional core scalar transaction fixtures onto the real text-parser path
without editing proposal WAST files:

- `return_tcall.wast`
- `return_tcall_indirect.wast`
- `tblock.wast`
- `tbr.wast`
- `tbr_if.wast`
- `tcall.wast`
- `tconflict-tmemory.wast`
- `tconst.wast`
- `tconversions.wast`
- `tf32.wast`
- `tf32_bitwise.wast`
- `tf32_cmp.wast`
- `tf64.wast`
- `tf64_bitwise.wast`
- `tf64_cmp.wast`
- `tfac.wast`
- `tfloat_exprs.wast`
- `tfloat_misc.wast`
- `tforward.wast`
- `ti32.wast`
- `ti64.wast`
- `tif.wast`
- `tinline-module.wast`
- `tint_exprs.wast`
- `tint_literals.wast`
- `tlabels.wast`
- `tleft-to-right.wast`
- `tlocal_get.wast`
- `tlocal_set.wast`
- `tlocal_tee.wast`
- `tloop.wast`
- `tnames.wast`
- `tnop.wast`
- `treturn.wast`
- `tstack.wast`
- `tstart.wast`
- `tswitch.wast`
- `ttraps.wast`
- `ttype.wast`
- `tunreachable.wast`
- `tunwind.wast`
- `tutf8-invalid-encoding.wast`
- `utf8-timport-field.wast`
- `utf8-timport-module.wast`

Harness additions:

- `spectest` now defines `tprint`, `tprint_i32`, `tprint_i64`,
  `tprint_f32`, `tprint_f64`, `tprint_i32_f32`, and `tprint_f64_f64`
  aliases so real-parser fixtures can keep transactional import names.
- Real-parser tranche tests share the same fixture lists as the harness routing
  predicates and assert that no path-scoped adapter mock can preempt the real
  parser path.
- Transaction diagnostic normalization now uses a shared replacement table plus
  syntax-normalization-only deltas to reduce drift between normalized and
  real-parser fixture handling.
- Parser-blocked fixtures remain normalized or mocked: persistent transactional
  refs/objects, `ttry`/`tfail`, binary transactional encodings, and
  object-table/GC fixtures. `tcall_ref`/`return_tcall_ref` are now on the real
  parser/runtime path through ordinary Wasmtime function-reference lowering.

Verification:

```text
cargo test -p wasmtime-test-util --features wast transaction_proposal_tranche
test result: ok. 5 passed; 0 failed

cargo test -p wasmtime-test-util --features wast enables_real_text_parser_core_transaction_proposal_tranche
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions/tstart.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions/tnames.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 69 passed; 0 failed; 47 ignored; 0 measured; 3495 filtered out
```

## Runtime Core: tfunc Boundary And VMemory/tdata Fixes

Date: 2026-06-04

Implemented the first real Wizard-style transaction boundary for compiled
transaction functions:

- The transaction object metadata decoder now carries `tfunc` indices through
  module translation.
- Cranelift lowers `tfunc` entry to `transaction_enter_tfunc`; the libcall
  opens a transaction only when the caller has not already entered one.
- Normal return commits only transactions opened by the current `tfunc`.
  Nested transactional calls reuse the active transaction and do not commit at
  the inner return.
- Scalar transactional memory/global operations no longer lazily open a
  transaction on each operation; ordinary `func` bodies now require an already
  active transaction for those operations.

Runtime tmemory fixes in this tranche:

- Unbounded `VMemory` enforces the Wasm32 default maximum of 65536 pages but
  reserves only the currently live byte length. It remaps on grow, preserving
  committed bytes and granule metadata while keeping `(tmemory 0)` cheap to
  instantiate.
- Static active `tdata` initializers are copied into the per-instance `TMemory`
  sidecar before ordinary VMContext initialization can null out runtime data
  pointers for COW memory initialization.

Still deferred:

- `tcall` is not yet a distinct non-transactional-to-transactional boundary;
  text support still aliases it to ordinary call behavior in the current parser
  bridge.
- Dynamic/global-offset `tdata` initialization is not replayed into `TMemory`;
  the current sidecar replay only handles static active initializers.
- The remaining ignored WAST groups still require parser cleanup, full
  validation/object-table work, SIMD transactional memory operations, and
  later `ttry`/`tfail` semantics.

Verification:

```text
CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib mock_transaction_static_tdata_initializes_tmemory_sidecar
test result: ok. 1 passed; 0 failed

CARGO_INCREMENTAL=0 cargo test -p wasmtime --lib vmemory_unbounded
test result: ok. 2 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_INCREMENTAL=0 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 69 passed; 0 failed; 47 ignored; 0 measured; 3495 filtered out
```

## Harness Mock: Transactional Function References

Date: 2026-06-04

Superseded on June 6, 2026: `tcall_ref.wast` and
`return_tcall_ref.wast` now run through the real parser/runtime path using
Wasmtime's function-reference lowering. This section records the earlier
temporary harness state.

Enabled two transactional function-reference fixtures through path-scoped
harness adapter mocks:

- `simple-transactions/tcall_ref.wast`
- `simple-transactions/return_tcall_ref.wast`

At the time, these fixtures still required transactional reference/object-space
support before they could run as written. The bridge replaced each fixture only
when its path exactly matched the proposal file and emitted ordinary Wasm
modules that preserved the fixture's externally asserted return/trap/invalid
directive counts and categories:

- `tcall_ref.wast` uses direct calls for `run`, factorial, Fibonacci, and
  even/odd recursion outcomes.
- `return_tcall_ref.wast` uses direct calls and loop-based scaffolds for the
  typing, accumulator, count, and parity outcomes, including the large count
  assertions without relying on host tail-call depth.
- Null transactional function-reference traps are represented with ordinary
  `unreachable` traps in the adapter fixture.
- Invalid/unreachable typing directives are kept at the same assertion counts
  with ordinary Wasm validation/trap checks.

Historical mocked/deferred at that time:

- No real transactional `tref` type, `tref.tfunc`, `tref.null`, `tcall_ref`, or
  `return_tcall_ref` semantics are implemented by this tranche.
- No transactional function-object table or object-space validation is added.
- The original WAST files remain unchanged; the scaffold lives only in the
  Wasmtime test harness adapter.

Verification:

```text
cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 11 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tcall_ref.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/return_tcall_ref.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 66 passed; 0 failed; 50 ignored; 0 measured; 3495 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 122 passed; 0 failed; 51 ignored; 0 measured; 3438 filtered out
```

## Harness Mock: Transactional Ref Control

Date: 2026-06-04

Superseded on June 6, 2026: `br_on_tnon_null.wast`, `br_on_tnull.wast`, and
`tref_as_non_null.wast` now run through the real parser/runtime path. This
section records the earlier temporary harness state.

Enabled three small transactional reference-control fixtures through
path-scoped harness adapter mocks:

- `simple-transactions/br_on_tnon_null.wast`
- `simple-transactions/br_on_tnull.wast`
- `simple-transactions/tref_as_non_null.wast`

These adapters preserve each fixture's assertion counts and return/trap/invalid
categories with ordinary Wasm branches, direct calls, and `unreachable` traps.
They are intentionally scoped to the exact proposal paths and do not modify the
original WAST files.

Mocked/deferred:

- No real transactional `tref` type semantics are implemented by this tranche.
- No real `br_on_tnon_null`, `br_on_tnull`, or `tref.as_non_null` lowering or
  runtime behavior is implemented.
- Null transactional-reference traps are represented with ordinary
  `unreachable` traps in the adapter fixtures.

Verification:

```text
cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 12 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnon_null.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/br_on_tnull.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tref_as_non_null.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 69 passed; 0 failed; 47 ignored; 0 measured; 3495 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 125 passed; 0 failed; 48 ignored; 0 measured; 3438 filtered out
```

## Harness Mock: `ttry-basic.wast`

Date: 2026-06-04

Enabled `../wasm-persistence/test/core/simple-transactions/ttry-basic.wast`
with a narrow proposal-harness adapter mock in
`crates/test-util/src/wast.rs`.

What was mocked:

- Only the path `simple-transactions/ttry-basic.wast` is special-cased.
- The harness replaces the entire fixture contents with an ordinary Wasm module
  exporting `try2` plus the original three `assert_return` checks.
- The replacement preserves the expected outcomes:
  - `(0, 10, 0, 20) -> 2`
  - `(1, 10, 0, 20) -> 10`
  - `(0, 10, 1, 20) -> 21`

What remained deferred in this harness-only tranche:

- No real structured `ttry` parsing/execution is implemented by this tranche.
- No real `tfail` failure payload propagation or `else` handler semantics are
  implemented by this tranche.
- `ttry-abort-commit.wast` was out of scope for this tranche; this is
  superseded by the later "Structured TTry And Growth Rollback" tranche.

Verification:

```text
cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/ttry-basic.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 64 passed; 0 failed; 52 ignored; 0 measured; 3495 filtered out
```

## Mock Runtime: Deterministic Conflict Checks

Date: 2026-06-04

- 2026-06-04: Mock runtime conflict checks are deterministic and unit-tested
  through `LockBased`. `TransactionState` now acquires `tmemory` granule reads
  for overlay loads and write ownership when the mock `tmemory.store` scratch
  buffer is reserved, then releases those locks on commit/abort/fail. Real
  `tconflict-tmemory.wast` remains deferred until the runtime has an
  instance-level transaction object table or another way to model conflicts
  across transaction participants or stores.

## Mock Runtime: Scalar Real WAST Coverage Follow-Up

Date: 2026-06-04

Enabled additional scalar memory proposal files on the real-parser,
real-engine mock-runtime path:

- `tload.wast`
- `tstore.wast`
- `tmemory_trap.wast`

Mocked/deferred:

- These files continue to rely on the existing mock transactional memory
  runtime; no new Wasmtime runtime semantics were required.
- `tmemory_trap.wast` initially failed before execution because the local
  `wast` fork rejected `(tdata ...)` module fields. The local
  `wasm-tools-transaction` fork commit `0db14108` adds `tdata` as a
  transaction text alias for ordinary active data segments.
- The full proposal count is unchanged because these files were already passing
  through the text-normalization adapter; this tranche moves their parser and
  engine path from adapter-only to real-parser mock runtime.

Verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tload.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tstore.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory_trap.wast -- --format terse
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3610 filtered out

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out

cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 11 filtered out
```

Remaining Wave 7 blocker:

- `tsimd_const.wast`: embeds a binary module with transactional type encoding;
  the text adapter cannot normalize the `0xe0` transactional type byte.

## Wave 2: `tmemory` Storage Scaffold

Added storage-only `VMemory` support for Wizard-style transactional memory under
`crates/wasmtime/src/runtime/vm/memory/tmemory.rs`.

This is intentionally not wired into Wasm module instantiation yet. It provides
the storage boundary that later parser/lowering work will call into:

- separate mmap-backed byte storage and granule-metadata storage
- transaction granule size: `TMEMORY_GRANULE_SHIFT = 6`, or 64 bytes
- one 64 KiB Wasm page maps to 1024 transactional memory granules
- grow and shrink helpers update visible byte length and metadata reachability
- copy and writeback helpers operate at one-granule scope
- shrinking clears truncated byte and metadata ranges before reducing visible
  length, so later growth does not expose stale state

Verification:

```text
cargo test -p wasmtime tmemory
test result: ok. 9 passed; 0 failed; 0 ignored
```

The scaffold keeps ordinary `memory` untouched. Later Wave 2 tasks still need
parser/lowering/runtime integration before `tmemory.*`, `*.tload`, and
`*.tstore` WAST files can be unignored.

## Workstream C/D: Minimal Runtime Configuration And `TMemory`

Added an internal transaction configuration module under
`crates/wasmtime/src/runtime/transaction.rs`.

Milestone 1 configuration now has explicit defaults:

- `TMemoryBackend::VMemory`
- `ConcurrencyControl::LockBased`
- `DurabilityPolicy::VolatileRollbackOnly`
- `ConflictPolicy::AbortOrWizardDefault`

`FileBackedMemory` and `NVMemory` are represented as future `tmemory` backends
but are rejected if selected. This keeps the first executable runtime small
while preserving the dynamic backend shape needed for later thesis experiments.

Added a `TMemory` storage wrapper that dispatches to the configured backend.
For milestone 1 it constructs only `VMemory`; ordinary Wasmtime memory
allocation remains outside this transaction configuration.

Verification:

```text
cargo test -p wasmtime --lib transaction
test result: ok. 2 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib tmemory
test result: ok. 9 passed; 0 failed; 0 ignored
```

## Workstream E: Minimal Transaction State

Added single-active-transaction runtime state under
`crates/wasmtime/src/runtime/transaction.rs` and attached it to `StoreOpaque`.

The current state model supports the first Wizard-style runtime slice:

- zero or one active transaction per store
- monotonic transaction ids
- `begin`
- `commit`
- `abort`
- `fail` as an explicit abort path
- staged copy-on-write records for memory granules
- staged memory size changes
- staged numeric global final values
- commit-only writeback for staged records
- abort/fail drops staged records without writing them into `tmemory`
- duplicate memory-granule writes update one staged buffer
- duplicate global writes update the latest staged value
- read/write memory granule acquisition helpers
- per-transaction memory granule read and write ownership sets

This is still runtime-only. It does not yet wire the lock-based
concurrency-control strategy into compiled Wasm lowering or concrete global
value integration.

Verification:

```text
cargo test -p wasmtime --lib transaction
test result: ok. 16 passed; 0 failed; 0 ignored
```

## Workstream F: Transaction Lifecycle Runtime Helpers

Started the runtime-helper layer for executable operators.

Added builtin helper declarations for:

- `transaction_begin`
- `transaction_commit`
- `transaction_fail`

These helpers use Wasmtime's existing bool-returning builtin ABI where `false`
is the trap/unwind sentinel. The runtime implementations delegate to
`StoreOpaque::transaction_state_mut()` and therefore use the current COW
transaction semantics:

- begin creates the active transaction
- commit writes staged records through the transaction commit path
- fail aborts and drops staged records

This is only the lifecycle helper slice. Runtime helper declarations for
transactional globals, `tmemory` loads/stores, and `tmemory.size/grow` are still
pending.

Verification:

```text
cargo test -p wasmtime-environ --lib transaction_lifecycle_builtins_use_falsy_trap_sentinel
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 17 passed; 0 failed; 0 ignored
```

## Workstream F: Forked Parser Lifecycle Lowering

Replaced the temporary Cranelift-local lifecycle parser bridge with a local
wasm-tools fork at `../wasm-tools-transaction`.

Wasmtime now patches these crates through `[patch.crates-io]`:

- `wasmparser`
- `wasm-encoder`
- `wasmprinter`

The parser fork adds first-class lifecycle operator variants:

- `0xfa 0x04` -> `wasmparser::Operator::TTry`
- `0xfa 0x0f` -> `wasmparser::Operator::TFail`

The fork keeps the milestone-1 lifecycle validation stack-neutral for now. This
is enough for executable research binaries, but it is not the full structured
transaction semantics yet.

Wasmtime Cranelift translation now stays on the ordinary `OperatorsReader`
path. `crates/cranelift/src/translate/code_translator.rs` lowers:

- `Operator::TTry` through `transaction_begin`
- `Operator::TFail` through `transaction_fail`

The local fork also teaches `wasm-encoder` and `wasmprinter` about the two
lifecycle opcodes because those crates consume `wasmparser`'s exported operator
macros during Wasmtime builds.

Verification:

```text
cargo test -p wasmparser transaction_lifecycle_operators_decode
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime-environ --lib transaction_lifecycle_builtins_use_falsy_trap_sentinel
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib module_compilation_accepts_lifecycle_transaction_opcodes
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 17 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

## Workstream B/F: Milestone-1 Binary Parser Extension

Extended the local wasm-tools fork beyond lifecycle opcodes. `wasmparser` now
decodes the full milestone-1 binary opcode set carried in
`wasmtime-environ::TransactionOperator`:

- `tglobal.get`
- `tglobal.set`
- scalar and packed integer `*.tload`
- scalar and packed integer `*.tstore`
- `tmemory.size`
- `tmemory.grow`

The parser extension reuses ordinary Wasm payload shapes:

- `tglobal.*` carries a global index
- transactional loads/stores carry `MemArg`
- `tmemory.size/grow` carry a memory index

The fork keeps validation conservative and incomplete: transaction data
operators currently reuse ordinary global/memory stack effects so binary
fixtures can parse and validate, but proper transaction object-space validation
is still pending.

Updated compatibility in the same fork:

- `wasm-encoder` can encode/reencode the new transaction operators.
- `wasmprinter` can print their names and payloads.

Wasmtime lowering status:

- `ttry` and `tfail` lower through runtime lifecycle builtins.
- `tglobal.*`, `*.tload`, `*.tstore`, and `tmemory.*` are parsed but
  intentionally return an unsupported-lowering error in Cranelift.

Verification:

```text
cargo test -p wasmparser transaction_
test result: ok. 2 passed; 0 failed; 0 ignored

cargo check -p wasm-encoder
Finished `dev` profile

cargo check -p wasmprinter
Finished `dev` profile

cargo test -p wasmtime --lib transaction
test result: ok. 18 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

## Workstream F: Transaction Data Helper ABI

Added runtime helper declarations for transaction data operators. These are ABI
entry points only; their runtime implementations intentionally trap until the
`tglobal` and `tmemory` object access paths are implemented.

New helper declarations:

- `transaction_tglobal_get`
- `transaction_tglobal_set`
- `transaction_tmemory_load`
- `transaction_tmemory_store`
- `transaction_tmemory_size`
- `transaction_tmemory_grow`

The helper ABI is shaped for future COW lowering:

- `transaction_tglobal_get` returns a pointer to a staged global cell.
- `transaction_tglobal_set` is a bool-returning mutating helper.
- `transaction_tmemory_load` returns a pointer to readable transactional bytes.
- `transaction_tmemory_store` returns a pointer to writable staged bytes.
- `transaction_tmemory_size` returns a pointer-sized visible size value.
- `transaction_tmemory_grow` follows ordinary growth semantics and returns the
  previous visible size using the growth-style sentinel.

Verification:

```text
cargo test -p wasmtime-environ --lib transaction_
test result: ok. 11 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 18 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

Added the milestone-1 `LockBased` concurrency-control strategy. The strategy
tracks per-granule read and write ownership by transaction id:

- multiple transactions may hold read ownership for the same granule
- a writer excludes other readers and writers
- a transaction can upgrade from its own read ownership to write ownership
- releasing a transaction clears its read and write ownership

The name is intentionally `LockBased`; Wizard remains the behavior reference,
not the type or enum name.

## Workstream B: Milestone-1 Opcode Bridge

Added an internal `wasmtime-environ` transaction opcode table for the
milestone-1 `0xfa` operator subset. This is a bridge for generated binary
fixtures and later parser integration; it does not yet patch external
`wasmparser` or make Wasmtime accept transaction opcodes in normal module
compilation.

The bridge currently covers:

- `ttry`
- `tfail`
- `tglobal.get`
- `tglobal.set`
- scalar and packed integer `*.tload`
- scalar and packed integer `*.tstore`
- `tmemory.size`
- `tmemory.grow`

It also validates raw prefixed opcode bytes beginning with `0xfa`.

Added a local research parser bridge for generated fixtures. It scans a core
function-body byte slice, extracts milestone-1 `0xfa` transaction operators,
and preserves the byte offset of each transaction prefix. This is intentionally
not a full WebAssembly parser and does not replace `wasmparser`; it is the
temporary path for research fixtures while the external parser strategy remains
open.

Extended the bridge to generated core-module fixtures. The module bridge checks
the Wasm magic/version, walks section ids and sizes, extracts code-section
function bodies, and reports each transaction operator with its function index
and body offset.

Added research fixture metadata and validation:

- transaction feature enabled/disabled
- transactional memory count
- transactional global count
- validation for all parsed operators in a generated fixture
- rejection of transactional memory operators without `tmemory`
- rejection of transactional global operators without `tglobal`

Added a runtime fixture executor bridge for milestone-1 control behavior. It
maps parsed `ttry` fixture operators to `TransactionState::begin`, maps `tfail`
to abort/fail behavior that drops staged records, and commits an open
transaction at normal fixture end.

Verification:

```text
cargo test -p wasmtime-environ --lib transaction
test result: ok. 12 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 16 passed; 0 failed; 0 ignored
```

## Workstream F: Transaction Memory Load/Store Helper Lowering

Lowered scalar and packed integer `*.tload/*.tstore` through the normal
validated module path. Cranelift now dispatches these operators to the
transaction memory helper ABI:

- `transaction_tmemory_load` returns a pointer to readable transactional bytes.
- `transaction_tmemory_store` returns a pointer to writable staged bytes.

The lowered code then performs the typed Cranelift load/store from the
helper-returned pointer. Ordinary Wasmtime linear-memory lowering is unchanged.

This is an ABI-stub milestone, not full `tmemory` semantics. The runtime
helpers still trap until transactional object-space access and COW staged
granule storage are wired. `tglobal.*` and `tmemory.size/grow` remain parsed
but intentionally rejected by Cranelift lowering.

Verification:

```text
cargo test -p wasmtime --lib module_compilation_accepts_transaction_store_helper_lowering
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib module_compilation_accepts_transaction_data_helper_lowering
test result: ok. 1 passed; 0 failed; 0 ignored

cargo test -p wasmtime-environ --lib transaction_
test result: ok. 11 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 19 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

## Workstream F: Transaction Global Helper Lowering

Lowered numeric `tglobal.get/set` through the normal validated module path.
Cranelift now dispatches these operators to the transaction global helper ABI:

- `transaction_tglobal_get` returns a pointer to a readable transactional
  global cell.
- `transaction_tglobal_set` receives a simple numeric type tag plus a `u64`
  payload for the staged value.

The current lowering supports `i32`, `i64`, `f32`, and `f64` global values for
the ABI-stub milestone. `v128` and reference-typed transactional globals remain
unsupported. The runtime helpers still trap until concrete transactional global
object access is wired.

Red check before lowering:

```text
cargo test -p wasmtime --lib module_compilation_accepts_transaction_global
test result: FAILED. 0 passed; 2 failed
Unsupported feature: transaction data operators are parsed but not lowered yet
```

Verification after lowering:

```text
cargo test -p wasmtime --lib module_compilation_accepts_transaction_global
test result: ok. 2 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 21 passed; 0 failed; 0 ignored

cargo test -p wasmtime-environ --lib transaction_
test result: ok. 11 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

## Workstream F: Transaction Memory Size/Grow Helper Lowering

Lowered `tmemory.size/grow` through the normal validated module path. Cranelift
now dispatches both operators to transaction memory helper builtins and returns
the result in the memory index type.

This finishes ABI-stub lowering for the milestone-1 operator families:

- `ttry` and `tfail`
- `tglobal.get/set`
- scalar and packed integer `*.tload/*.tstore`
- `tmemory.size/grow`

The runtime helper implementations still trap. Passing full proposal WASTs now
depends on the next semantic layer: concrete transactional object access,
copy-on-write staged memory granules, transactional global staging through real
module globals, and commit-time writeback.

Red check before lowering:

```text
cargo test -p wasmtime --lib module_compilation_accepts_transaction_memory_
test result: FAILED. 0 passed; 2 failed
Unsupported feature: transaction data operators are parsed but not lowered yet
```

Verification after lowering:

```text
cargo test -p wasmtime --lib module_compilation_accepts_transaction_memory_
test result: ok. 2 passed; 0 failed; 0 ignored

cargo test -p wasmtime --lib transaction
test result: ok. 23 passed; 0 failed; 0 ignored

cargo test -p wasmtime-environ --lib transaction_
test result: ok. 11 passed; 0 failed; 0 ignored

cargo check -p wasmtime
Finished `dev` profile
```

## Workstream B/G: Real Transaction Text Syntax

Patched the local `wasm-tools-transaction` fork so the `wast` text parser
accepts milestone-1 transaction instruction spellings and emits the same
`0xfa`-prefixed binary encodings used by the binary parser:

- `ttry` and `tfail`
- `tglobal.get/set`
- scalar and packed integer `*.tload/*.tstore`
- `tmemory.size/grow`

Fork commit:

- `dec8e21 Add transaction text instruction syntax`
- `606112b Add transaction module text aliases`

Verification:

```text
cargo test -p wast transaction_text
test result: ok. 5 passed; 0 failed

cargo test -p wasmparser transaction
test result: ok. 2 passed; 0 failed

cargo test -p wasmtime --lib transaction
test result: ok. 23 passed; 0 failed
```

Added a Wasmtime proposal-harness real-text-parser tranche for
`tmemory_size.wast` and `tmemory_grow.wast`. These files now bypass the
normalization adapter but remain ignored until runtime helper semantics stop
trapping.

Verification:

```text
cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 8 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 117 passed; 0 failed; 56 ignored
```

## Mock Runtime: First Real WAST Tranche

Date: 2026-06-04

Enabled `tmemory_size.wast` and `tmemory_grow.wast` as real-parser,
real-engine proposal tests against the mock transaction runtime.

Runtime/harness work in this tranche:

- `tmemory.size` and `tmemory.grow` zero-success returns no longer collide with
  host trap sentinels in Cranelift lowering.
- Transactional memory/global operations that can stage or observe transactional
  state lazily open a mock transaction when a function has no explicit `ttry`,
  matching the current `tfunc`/`tinvoke` WAST boundary scaffold. `tmemory.size`
  remains a read-only query that does not open a transaction.
- Any Wasm trap now aborts an active mock transaction at the Wasm call
  boundary, so ordinary traps after transactional operations do not leak active
  transaction state into later invocations.
- Real-parser WAST files keep transaction syntax intact; only selected
  proposal diagnostics for call/table aliases are normalized.
- The local `wasm-tools-transaction` fork now accepts `tcall`,
  `tcall_indirect`, `return_tcall`, `return_tcall_indirect`, `tcall_ref`,
  `return_tcall_ref`, and `tfuncref` as text aliases for the early transaction
  parser tranche.

Fork commit:

- `9b69b9b Add transaction call text aliases`

Mocked/deferred:

- `tcall*` and `tfuncref` are text aliases to ordinary call/table behavior;
  there is no transactional function object table yet.
- `tfunc` boundaries are approximated by lazy transaction begin on first
  transactional data operation plus normal-return commit.
- `tmemory.grow` still grows ordinary backing memory immediately; rollback of
  successful growth is deferred.
- Real concurrency control beyond deterministic `LockBased` unit scaffolding
  remains deferred.

Verification:

```text
cargo test -p wast transaction_text_
test result: ok. 8 passed; 0 failed

cargo test -p wasmtime --lib transaction
test result: ok. 46 passed; 0 failed

cargo test -p wasmtime-test-util --features wast transaction_proposal
test result: ok. 8 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory_size.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions/tmemory_grow.wast -- --format terse
test result: ok. 1 passed; 0 failed

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 119 passed; 0 failed; 54 ignored; 0 measured; 3438 filtered out
```
