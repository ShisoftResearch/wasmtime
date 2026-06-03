# Transactional WebAssembly Implementation Log

This log records execution state for the Wizard-first transactional Wasmtime
research branch.

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

Remaining Wave 1 blockers:

- `tfloat_literals.wast`: embeds a binary module using transactional encodings.
- `tcall_ref.wast`, `return_tcall_ref.wast`, `tlocal_init.wast`,
  `tselect.wast`, `tunreached-valid.wast`, and `tunreached-invalid.wast`:
  blocked on transactional reference/function-reference validation.
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

Current proposal harness result:

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

Remaining Wave 7 blocker:

- `tsimd_const.wast`: embeds a binary module with transactional type encoding;
  the text adapter cannot normalize the `0xe0` transactional type byte.

## Wave 2: `tmemory` Storage Scaffold

Added storage-only `VMemory` support for Wizard-style transactional memory under
`crates/wasmtime/src/runtime/vm/memory/tmemory.rs`.

This is intentionally not wired into Wasm module instantiation yet. It provides
the storage boundary that later parser/lowering work will call into:

- separate mmap-backed byte storage and granule-metadata storage
- Wizard granule size: `TMEMORY_GRANULE_SHIFT = 8`, or 256 bytes
- one 64 KiB Wasm page maps to 256 transactional memory granules
- grow and shrink helpers update visible byte length and metadata reachability
- snapshot and restore helpers operate at one-granule scope
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
- undo records for memory granule snapshots
- undo records for memory size restore
- undo records for numeric global restore
- reverse-order undo traversal on abort/fail
- duplicate memory-granule writes record only one snapshot
- duplicate global writes record only one restore snapshot
- read/write memory granule acquisition helpers
- per-transaction memory granule read and write ownership sets

This is still runtime-only. It does not yet wire the lock-based
concurrency-control strategy into compiled Wasm lowering or concrete global
value integration.

Verification:

```text
cargo test -p wasmtime --lib transaction
test result: ok. 13 passed; 0 failed; 0 ignored
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

Verification:

```text
cargo test -p wasmtime-environ --lib transaction
test result: ok. 8 passed; 0 failed; 0 ignored
```
