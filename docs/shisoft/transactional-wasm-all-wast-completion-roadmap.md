# Transactional Wasm All-WAST Completion Roadmap

Date: 2026-06-06

## Goal

Move the transaction proposal WAST corpus from the current partial runtime to a
fully real Wasmtime execution path:

- no ignored transaction proposal WAST tests
- no WAST fixture replacement
- no transaction proposal text normalization
- no mock runtime behavior for proposal-visible semantics

The current baseline is:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 127 passed; 0 failed; 46 ignored
```

## Execution Status

2026-06-06:

- Wave 1 foundation started.
- Added the first in-memory `ObjectTable` runtime foundation with dense stable
  `ObjectId` allocation, freed-slot reuse, live-slot versions, and kind-aware
  `TStruct`/`TArray` granule acquisition.
- Added staged struct/array payload COW, abort discard, commit application, and
  optimistic object read-version validation.
- Focused verification:

```text
cargo test -p wasmtime --lib transaction -- --format terse
test result: ok. 84 passed; 0 failed; 0 ignored
```

2026-06-06 later update:

- Wave 2/4 parser-runtime bridge expanded for reference-valued globals and
  tables.
- Fixed the local `wasm-tools-transaction` fork so `(table ... (tref null $t))`
  emits transactional table metadata.
- Fixed transactional const-expression handling for `tglobal.get`.
- Fixed named transactional data segment resolution for `tmemory.init $p` and
  `tdata.drop $p`.
- Moved these files onto the real parser/engine path without WAST edits:
  `tref_is_null.wast`, `tref_tfunc.wast`, `tglobal.wast`, `tbulk.wast`,
  `tdata.wast`, `telem.wast`, `tbr_table.wast`, `tfunc.wast`,
  `tlinking.wast`, `tendianness.wast`, `tfloat_literals.wast`,
  `ttable.wast`, `ttable-sub.wast`, and all focused `ttable_*.wast` files.
- Follow-up moved `tsimd_const.wast`, `tbinary.wast`,
  `tbinary-leb128.wast`, `tunreached-valid.wast`, and
  `tunreached-invalid.wast` onto the real parser/binary paths without WAST
  edits.
- Current verification:

```text
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal/simple-transactions -- --format terse
test result: ok. 114 passed; 0 failed; 2 ignored

WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
test result: ok. 171 passed; 0 failed; 2 ignored
```

Remaining simple-transaction ignored files are now concentrated in later
semantic waves:

- conflict/concurrency: `tconflict-basic.wast`, `tconflict-tmemory_1.wast`

There are no ignored files outside `simple-transactions` in the current
proposal harness run.

2026-06-06 object ABI update:

- Added shared runtime encoding for persistent object references: null is `0`,
  non-null is `object_index + 1`.
- Added a tagged two-word `ObjectValueAbi` scaffold for future `tstruct` and
  `tarray` libcalls, including scalar, reference, and `v128` payloads.
- Unified object heap record serialization with the same object-reference ABI.

2026-06-06 object bridge update:

- Added a volatile `VMGcRef` to `ObjectId` association bridge so compiled
  `tstruct.new/set/get` can exercise the in-memory `ObjectTable` without
  changing Wasmtime's ordinary GC representation.
- Promoted `ti31.wast` onto the real parser/runtime path by accepting
  `tref.ti31` in const expressions as an alias of ordinary `ref.i31`.
- At that checkpoint, full `tstruct.wast` still remained ignored because global
  initializers hit const-expression `TStructNew` and persistent-object
  allocation/abort logging was not complete. It is moved in the later
  object/reference completion update below.

2026-06-07 object bridge update:

- Moved `tarray.wast` onto the real parser/runtime path without WAST edits.
- Added transaction-aware `tarray.new_data` and `tarray.new_elem` recording so
  new arrays are associated with stable `ObjectId`s after ordinary Wasmtime GC
  allocation.
- Allowed non-i31 GC reference array elements through the temporary
  `VMGcRef -> ObjectId` bridge. Function-reference payloads are moved through a
  volatile `VMFuncRef -> ObjectId` bridge in the later object/reference
  completion update; the final persistent function-object path still needs
  durable `ObjectId` records.

2026-06-07 object permission update:

- Moved `tarray_fill.wast` onto the real parser/runtime path without WAST
  edits.
- The local parser fork now carries `tref none/read/write` as validator
  type-state. `tref.cast_read/write` are distinct parser operators, but
  Wasmtime lowers them as identity because permission is transaction state, not
  reference identity.
- `tarray.fill` now stages array payload changes through the transaction
  `ObjectId` bridge and aborts the active transaction on runtime errors.
- `tref.null` validates as permission-top type-state because null has no object
  granule to acquire.

2026-06-07 object array-copy update:

- Moved `tarray_copy.wast` onto the real parser/runtime path without WAST
  edits.
- `TArrayCopy` now uses a transaction-specific lowering and libcall instead of
  the ordinary Wasmtime GC array-copy path.
- Runtime copy resolves source/destination `VMGcRef`s through the temporary
  `ObjectId` bridge, stages the destination array payload through object COW,
  and preserves overlap semantics by copying through an intermediate vector.
- At that checkpoint, proposal verification was `157 passed; 0 failed; 16 ignored`,
  with all ignored files still under `simple-transactions`.

2026-06-07 object array-init-data update:

- Moved `tarray_init_data.wast` onto the real parser/runtime path without WAST
  edits.
- `TArrayInitData` now uses transaction-specific lowering and a libcall that
  stages decoded data bytes into the object COW payload.
- The runtime checks destination tarray bounds before source data bounds and
  treats the data source offset as a byte offset.
- At that checkpoint, proposal verification was `158 passed; 0 failed; 15 ignored`,
  with all ignored files still under `simple-transactions`.

2026-06-07 object/reference completion update:

- Moved `tarray_init_elem.wast`, `tstruct.wast`, `tref_eq.wast`,
  `textern.wast`, `tref_test.wast`, `tref_cast.wast`,
  `br_on_tcast.wast`, `br_on_tcast_fail.wast`, and the `ttype-*.wast`
  object/type fixtures onto the real parser/runtime path without WAST edits.
- Added the transaction-specific `TArrayInitElem` lowering/libcall. It decodes
  passive element-segment `ValRaw` entries, stages reference array payloads
  through object COW, and preserves proposal trap ordering.
- Added a volatile `VMFuncRef -> ObjectId` bridge for function references in
  transactional object payloads. This keeps the payload identity in
  `ObjectId` form while compiled Wasmtime calls still require the raw
  `VMFuncRef` pointer.
- `ttry-abort-commit.wast` now runs on the real parser/runtime path with
  structured `tfail` rollback and staged table/memory growth.
- Current proposal verification is `171 passed; 0 failed; 2 ignored`, with all
  ignored files still under `simple-transactions`.

## Ground Rules

- Do not edit proposal WAST test cases.
- Harness changes are allowed only to remove ignores, remove normalization, or
  route tests through real parser/runtime support.
- Wizard semantics are the source of truth where the proposal and tests are
  ambiguous.
- Keep ordinary Wasmtime `memory`, ordinary table, and ordinary GC behavior
  unchanged unless a transaction operator explicitly crosses into it.
- `VMemory` remains the only required storage backend for WAST completion.
  `FileBackedMemory` and `NVMemory` are not WAST blockers.
- Every implementation wave must add failing unit or WAST-harness tests before
  production code.

## Wave 1: Object Table And Transactional Reference Foundation

Purpose: make `ObjectId` and object granules real runtime entities instead of
reserved enum variants.

Implement:

- `ObjectTable` with dense stable `ObjectId` allocation, freelist reuse, live
  slot lookup, update, free, and per-slot version metadata.
- object kind metadata for `TStruct`, `TArray`, `TI31`, `TExtern`, and `TFunc`.
- object granule helper methods that map object slots to
  `GranuleId::TStruct` and `GranuleId::TArray` for permission and conflict
  acquisition.
- transaction state integration so object reads use optimistic read permission
  and object writes use pessimistic write permission.

Initial tests:

- allocation returns stable dense object ids
- freeing a slot reuses that id before growing the table
- struct and array permission helpers acquire the correct granule kind
- abort releases object ownership
- commit validates object read versions

Exit criteria:

- `cargo test -p wasmtime --lib transaction -- --format terse` passes.
- `TStruct` and `TArray` are no longer "reserved only" in runtime docs.

## Wave 2: Transactional Reference Types, Validation, And Lowering

Purpose: remove the largest parser/validation blocker for ignored
simple-transaction WAST files.

Implement:

- real text parsing for transactional heap types and value types:
  `tref`, `tanyref`, `teqref`, `ti31ref`, `tstructref`, `tarrayref`,
  `texternref`, `tfuncref`, and nullable/non-null forms
- Wizard permission type surface: `none`, `read`, and `write`
- validation for Wizard permission subtyping: write is usable where read is
  required, read is usable where none is required, and the reverse is rejected
- lowering for non-mutating ref operators:
  `tref.null`, `tref.is_null`, `tref.eq`, `tref.as_non_null`, `tref.test`,
  `tref.cast`, `br_on_tnull`, `br_on_tnon_null`, `br_on_tcast`, and failure
  variants
- `tcall_ref` and `return_tcall_ref` through real transactional function refs

Primary WAST targets:

- `tref*.wast`
- `br_on_t*.wast`
- `tcall_ref.wast`
- `return_tcall_ref.wast`
- `tselect.wast`
- `tlocal_init.wast`
- `tunreached-valid.wast`
- `tunreached-invalid.wast`

Exit criteria:

- path-scoped replacements for `br_on_tnon_null`, `br_on_tnull`,
  `tref_as_non_null`, `tcall_ref`, and `return_tcall_ref` are removed.
- affected files run through the real text parser.

## Wave 3: Transactional Struct, Array, I31, And Extern Runtime

Purpose: implement the object-model operations that consume the object table.

Implement:

- transactional struct creation, field get/set, null checks, and permission
  acquisition
- transactional array creation, get/set, len, fill, copy, init-data, init-elem,
  and permission acquisition
- transactional i31 creation and signed/unsigned access
- transactional extern conversion and cast/test behavior
- object value snapshots sufficient for abort and conflict validation

Primary WAST targets:

- `tstruct.wast`
- `tarray*.wast`
- `ti31.wast`
- `textern.wast`
- `ttype-rec.wast`
- `ttype-subtyping.wast`
- `ttype-canon.wast`
- `ttype-equivalence.wast`

Exit criteria:

- object-model WAST files run without normalization.
- object writes abort cleanly without mutating committed object state.

## Wave 4: Reference-Valued Globals, Tables, Elements, And Data

Purpose: finish object/reference values in globals and tables.

Implement:

- `GlobalSnapshot` support for transactional reference/object values
- imported and defined ref-valued `tglobal` get/set
- transactional table type metadata beyond the current funcref smoke path
- table element COW workspace so abort discards `ttable.set/fill/copy/init`
- `telem.drop` and `tdata.drop` transactional ownership where the WAST expects
  rollback

Primary WAST targets:

- `tglobal.wast`
- `ttable*.wast`
- `telem.wast`
- `tdata.wast`
- `tbulk.wast`
- `tfunc.wast`
- `tlinking.wast`

Exit criteria:

- table `SHISOFT-TWASM-MOCK` comments in Cranelift lowering are removed or
  converted to non-proposal-invisible backend notes.
- no table WAST remains blocked on ref-valued table metadata.

## Wave 5: Structured `ttry` And `tfail`

Purpose: replace the path-scoped `ttry-basic.wast` harness mock with real
control-flow semantics.

Implement:

- real structured `ttry` lowering
- `tfail` as transaction failure control flow, not a generic trap
- handler/else behavior following Wizard or the proposal where Wizard is
  incomplete
- abort of staged memory, globals, tables, and objects on failure
- normal return commit when no `tfail` reaches the handler

Primary WAST targets:

- `ttry-basic.wast`
- `ttry-abort-commit.wast`

Exit criteria:

- `transaction_proposal_adapter_mock` has no `ttry-basic.wast` replacement.
- `ttry` and `tfail` are in the real parser/lowering allowlist.

## Wave 6: Conflict And Multi-Transaction Harness Semantics

Purpose: make concurrency WAST files test real LockBased behavior.

Status: in progress. The runtime now has selected transaction-id workspaces and
the proposal `spectest` helpers enter/restore selected tids around `run_as_tid`
and route explicit abort/commit into runtime state. The remaining WAST blockers
are fixture-shape and transaction-boundary issues, not just missing lock
ownership.

Implement:

- real transaction spectest helper semantics for multiple transaction ids
  (partially complete for selected runtime tid switching)
- object/table/memory conflict routes through one `GranuleId` permission layer
- deterministic abort on conflict, without automatic retry
- conflict tests for optimistic read validation and pessimistic write ownership

Primary WAST targets:

- `tconflict-basic.wast`
- `tconflict-tmemory.wast`
- `tconflict-tmemory_1.wast`

Exit criteria:

- conflict WAST files no longer rely on smoke-only behavior.
- `crates/wast/src/spectest.rs` transaction helper mocks are either removed or
  narrowed to non-semantic test harness plumbing.
- ordinary conflict helper setup either moves behind an explicit transaction
  boundary or has a documented Wizard-compatible rule for t-prefixed object
  construction in helper functions.

## Wave 7: Binary Transaction Encodings

Purpose: support proposal binary modules directly.

Status: complete for current WAST coverage. `tbinary.wast`,
`tbinary-leb128.wast`, and the binary-encoding portions exercised by
`tsimd_const.wast` now run without ignored flags or syntax normalization.

Implement in the local `wasm-tools-transaction` fork and Wasmtime integration:

- binary transactional type encodings such as `0xe0` tfunc/ref type bytes
- transactional memory/table/type flags used in proposal binary tests
- binary operator encodings for transaction-prefixed instructions
- equivalent Wasmtime diagnostics for invalid encodings and LEB edge cases

Primary WAST targets:

- completed: `tbinary.wast`, `tbinary-leb128.wast`, `tfloat_literals.wast`,
  `tsimd_const.wast`

Exit criteria:

- binary proposal WAST files no longer fail on unknown transaction type bytes.
- local wasm-tools fork carries real parser support rather than scaffold-only
  comments for these encodings.

## Wave 8: SIMD Numeric Alias Cleanup

Purpose: remove the remaining SIMD text normalization for non-memory SIMD
instructions.

Implement:

- real text aliases for transactional SIMD numeric instructions
- validation that they require transaction contexts only where Wizard/proposal
  marks them transactional
- lowering that reuses ordinary Wasmtime SIMD arithmetic after the transaction
  opcode is accepted

Primary WAST targets:

- `tsimd_*` files currently passing only through the adapter

Exit criteria:

- all `tsimd` files run without normalization.

## Wave 9: Harness And Documentation Cleanup

Purpose: prove completion.

Remove:

- `transaction_proposal_adapter_mock`
- transaction proposal token normalization
- path-scoped fixture replacements
- stale `SHISOFT-TWASM-MOCK` comments for proposal-visible behavior
- ignored flags for transaction proposal files

Final verification:

```text
cargo test -p wasmtime --lib transaction -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime-environ transaction_object_metadata
WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast transaction-proposal -- --format terse
rg "SHISOFT-TWASM-MOCK|transaction_proposal_adapter_mock|normaliz" crates tests docs/shisoft
git diff --check
```

Required final WAST result:

```text
test result: ok. 173 passed; 0 failed; 0 ignored
```
