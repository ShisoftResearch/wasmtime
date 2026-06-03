# Full Proposal WAST Roadmap

This roadmap covers the work required to pass the additional proposal WAST
tests after the Wizard-first milestone-1 foundation is in place.

Use `docs/shisoft/transactional-wasm-action-plan.md` for milestone 1. Use this
document when expanding from milestone 1 toward the full proposal corpus.

When implementing this roadmap, use subagent-driven-development: dispatch a
fresh implementer subagent per task or wave, then run a spec-compliance review
subagent and a code-quality review subagent before marking that task complete.

## Test Corpus

The proposal test corpus currently has:

- `116` files under `../wasm-persistence/test/core/simple-transactions`
- `57` files under `../wasm-persistence/test/core/tsimd`
- `173` total `.wast` files

Milestone 1 only targets the small executable core. This roadmap covers the
remaining test families needed to reach full proposal coverage.

## Execution Protocol

Use this protocol for each wave:

1. Pick one wave from this document.
2. Create a focused task list for the wave with exact files and expected tests.
3. Dispatch one implementer subagent for the smallest independent task.
4. Require the implementer to run the wave's targeted verification command.
5. Dispatch a spec-compliance reviewer subagent.
6. Dispatch a code-quality reviewer subagent.
7. Fix review findings before starting the next task.
8. Record passing and skipped WAST files in an implementation log.

Do not dispatch subagents across tightly coupled compiler/runtime tasks that
write the same files at the same time. Parallelism is appropriate for test
classification, fixture generation, docs, and isolated runtime unit tests.

## Tracking Rules

Maintain a test ledger at:

- `docs/shisoft/transactional-wasm-wast-ledger.md`

For every `.wast` file, record:

- path
- feature family
- status: `unstarted`, `blocked`, `partial`, `passing`, or `deferred`
- blocking subsystem
- exact verification command
- notes about Wizard-vs-OCaml expectation differences

The ledger is the source of truth for full-suite progress. Do not rely on memory
or informal notes.

## Wave 0: Test Ledger And Harness

Purpose: make the full suite measurable before trying to pass it.

Depends on:

- milestone-1 parser strategy

Tasks:

- [ ] Generate the initial WAST ledger with all 173 files.
- [ ] Group each file by feature family.
- [ ] Add a test-runner filter for transaction proposal tests.
- [ ] Add skip metadata for features not implemented yet.
- [ ] Add one command that reports pass/fail/skip counts for the proposal set.

Subagent split:

- subagent 1: classify simple-transaction files
- subagent 2: classify transactional SIMD files
- subagent 3: inspect Wasmtime WAST harness integration and propose filter path

Exit:

- every proposal `.wast` file appears in the ledger
- skipped tests are skipped intentionally with a blocking subsystem named

## Wave 1: Core Control, Calls, Locals, Numerics

Purpose: pass transactional versions of ordinary Wasm control-flow and numeric
tests once milestone-1 transaction lifecycle exists.

Files:

- `tblock.wast`
- `tbr.wast`
- `tbr_if.wast`
- `tbr_table.wast`
- `tcall.wast`
- `tcall_indirect.wast`
- `tcall_ref.wast`
- `return_tcall.wast`
- `return_tcall_indirect.wast`
- `return_tcall_ref.wast`
- `tconst.wast`
- `ti32.wast`
- `ti64.wast`
- `tf32.wast`
- `tf32_bitwise.wast`
- `tf32_cmp.wast`
- `tf64.wast`
- `tf64_bitwise.wast`
- `tf64_cmp.wast`
- `tconversions.wast`
- `tif.wast`
- `tloop.wast`
- `tlocal_get.wast`
- `tlocal_set.wast`
- `tlocal_tee.wast`
- `tlocal_init.wast`
- `treturn.wast`
- `tselect.wast`
- `tnop.wast`
- `ttraps.wast`
- `tunreachable.wast`
- `tunreached-valid.wast`
- `tunreached-invalid.wast`
- `tlabels.wast`
- `tleft-to-right.wast`
- `tfac.wast`
- `tint_exprs.wast`
- `tint_literals.wast`
- `tfloat_exprs.wast`
- `tfloat_literals.wast`
- `tfloat_misc.wast`

Implementation themes:

- transactional call context
- `tcall`/`return_tcall` control transfer
- flat transaction nesting behavior
- trap-to-abort behavior
- ordinary numeric op reuse inside transactional code

Subagent split:

- calls/control subagent
- locals/select/return subagent
- numeric expression test migration subagent

Exit:

- all Wave 1 files are passing or have a precise blocker in the ledger

## Wave 2: Transactional Memory Core And Bulk Memory

Purpose: complete `tmemory` behavior beyond milestone-1 scalar loads/stores.

Files:

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
- `tmemory_copy.wast`
- `tmemory_fill.wast`
- `tmemory_init.wast`
- `tdata.wast`
- `tbulk.wast`
- `tskip-stack-guard-page.wast`

Implementation themes:

- all scalar and packed transactional load/store variants
- alignment validation
- endian behavior
- traps abort transactions
- memory copy/fill/init over multiple 256-byte granules
- passive data segment transaction metadata

Subagent split:

- scalar memory op subagent
- bulk memory subagent
- trap/alignment/addressing subagent

Exit:

- `tmemory.*`, `tload`, `tstore`, and bulk memory files pass under Wizard
  256-byte granule behavior

## Wave 3: Transactional Tables, Elements, Imports, Exports

Purpose: add transactional table and element segment behavior.

Files:

- `ttable.wast`
- `ttable-sub.wast`
- `ttable_get.wast`
- `ttable_set.wast`
- `ttable_size.wast`
- `ttable_grow.wast`
- `ttable_fill.wast`
- `ttable_copy.wast`
- `ttable_init.wast`
- `telem.wast`
- `timports.wast`
- `texports.wast`
- `tlinking.wast`
- `tinline-module.wast`
- `tstart.wast`

Implementation themes:

- transactional table index space
- `TTABLE_GRANULE_SHIFT = 4`
- table size undo
- table granule undo
- active/passive transactional elem segments
- transactional imports and exports
- linker/spectest support for transactional externs

Subagent split:

- table storage/runtime subagent
- elem segment subagent
- import/export/linking test subagent

Exit:

- table and element WAST files pass with 16-entry granule behavior

## Wave 4: Transactional Type System And Binary/Text Grammar

Purpose: pass type, binary, name, UTF-8, and malformed/invalid tests.

Files:

- `tbinary.wast`
- `tbinary-leb128.wast`
- `ttype.wast`
- `ttype-canon.wast`
- `ttype-equivalence.wast`
- `ttype-rec.wast`
- `ttype-subtyping.wast`
- `tforward.wast`
- `tnames.wast`
- `tutf8-invalid-encoding.wast`
- `utf8-timport-field.wast`
- `utf8-timport-module.wast`
- `tfunc.wast`
- `tfunc_ptrs.wast`

Implementation themes:

- transactional function types
- transactional recursive type canonicalization
- transactional subtype/equivalence rules
- transactional import/export names
- malformed binary decoding behavior
- text grammar compatibility with Wizard/proposal syntax

Subagent split:

- binary decoder subagent
- type-system subagent
- text/name/import grammar subagent

Exit:

- type and binary WAST tests pass or identify an explicit proposal/Wizard
  difference to resolve

## Wave 5: Transactional References And GC Objects

Purpose: implement transactional refs, permissions, structs, arrays, and i31.

Files:

- `tref.wast`
- `tref_null.wast`
- `tref_is_null.wast`
- `tref_as_non_null.wast`
- `tref_eq.wast`
- `tref_test.wast`
- `tref_cast.wast`
- `tref_tfunc.wast`
- `br_on_tnull.wast`
- `br_on_tnon_null.wast`
- `br_on_tcast.wast`
- `br_on_tcast_fail.wast`
- `tstruct.wast`
- `tarray.wast`
- `tarray_copy.wast`
- `tarray_fill.wast`
- `tarray_init_data.wast`
- `tarray_init_elem.wast`
- `ti31.wast`
- `textern.wast`

Implementation themes:

- transactional reference types and permissions
- `tref.cast_read`
- `tref.cast_write`
- transactional function refs
- transactional struct and array storage
- object field undo
- object granule ownership
- i31 and extern conversions

Subagent split:

- refs and casts subagent
- transactional struct subagent
- transactional array subagent
- i31/extern subagent

Exit:

- transactional refs and GC object files pass with Wizard permission semantics

## Wave 6: Conflict And Concurrency Semantics

Purpose: pass tests that require real concurrency-control behavior.

Files:

- `tconflict-basic.wast`
- `tconflict-tmemory.wast`
- `tconflict-tmemory_1.wast`

Implementation themes:

- selected concurrency-control strategy
- write ownership
- read validation
- conflict failure behavior
- wait-die/wound-wait options if needed by tests
- deterministic test harness behavior

Subagent split:

- conflict test harness subagent
- lock-based CC subagent
- optimistic validation subagent, if required

Exit:

- conflict tests pass under the selected Wizard-compatible default

## Wave 7: Transactional SIMD

Purpose: pass the 57 transactional SIMD files under `test/core/tsimd`.

Files:

- `tsimd_address.wast`
- `tsimd_align.wast`
- `tsimd_bit_shift.wast`
- `tsimd_bitwise.wast`
- `tsimd_boolean.wast`
- `tsimd_const.wast`
- `tsimd_conversions.wast`
- `tsimd_f32x4.wast`
- `tsimd_f32x4_arith.wast`
- `tsimd_f32x4_cmp.wast`
- `tsimd_f32x4_pmin_pmax.wast`
- `tsimd_f32x4_rounding.wast`
- `tsimd_f64x2.wast`
- `tsimd_f64x2_arith.wast`
- `tsimd_f64x2_cmp.wast`
- `tsimd_f64x2_pmin_pmax.wast`
- `tsimd_f64x2_rounding.wast`
- `tsimd_i8x16_arith.wast`
- `tsimd_i8x16_arith2.wast`
- `tsimd_i8x16_cmp.wast`
- `tsimd_i8x16_sat_arith.wast`
- `tsimd_i16x8_arith.wast`
- `tsimd_i16x8_arith2.wast`
- `tsimd_i16x8_cmp.wast`
- `tsimd_i16x8_extadd_pairwise_i8x16.wast`
- `tsimd_i16x8_extmul_i8x16.wast`
- `tsimd_i16x8_q15mulr_sat_s.wast`
- `tsimd_i16x8_sat_arith.wast`
- `tsimd_i32x4_arith.wast`
- `tsimd_i32x4_arith2.wast`
- `tsimd_i32x4_cmp.wast`
- `tsimd_i32x4_dot_i16x8.wast`
- `tsimd_i32x4_extadd_pairwise_i16x8.wast`
- `tsimd_i32x4_extmul_i16x8.wast`
- `tsimd_i32x4_trunc_sat_f32x4.wast`
- `tsimd_i32x4_trunc_sat_f64x2.wast`
- `tsimd_i64x2_arith.wast`
- `tsimd_i64x2_arith2.wast`
- `tsimd_i64x2_cmp.wast`
- `tsimd_i64x2_extmul_i32x4.wast`
- `tsimd_int_to_int_extend.wast`
- `tsimd_lane.wast`
- `tsimd_linking.wast`
- `tsimd_load.wast`
- `tsimd_load_extend.wast`
- `tsimd_load_splat.wast`
- `tsimd_load_zero.wast`
- `tsimd_load8_lane.wast`
- `tsimd_load16_lane.wast`
- `tsimd_load32_lane.wast`
- `tsimd_load64_lane.wast`
- `tsimd_splat.wast`
- `tsimd_store.wast`
- `tsimd_store8_lane.wast`
- `tsimd_store16_lane.wast`
- `tsimd_store32_lane.wast`
- `tsimd_store64_lane.wast`

Implementation themes:

- transactional SIMD load/store encodings
- SIMD lane loads and stores with granule snapshotting
- SIMD alignment/address validation
- reuse of existing Wasmtime SIMD arithmetic lowering
- transactional memory access wrappers around SIMD memory ops

Subagent split:

- SIMD parser/validation subagent
- SIMD transactional load/store subagent
- SIMD arithmetic/control migration subagent

Exit:

- all transactional SIMD files pass with transaction-aware memory operations

## Full-Suite Milestones

### FW0: Ledger Complete

Exit:

- all 173 files classified
- all files have status and blocking subsystem
- full-suite test filter exists

### FW1: Core Simple Transactions

Exit:

- Waves 1 and 2 pass
- all milestone-1 and memory/bulk memory tests pass

### FW2: Tables And Type System

Exit:

- Waves 3 and 4 pass
- transactional tables, elems, imports, exports, type, binary, and text grammar
  tests pass

### FW3: References And Objects

Exit:

- Wave 5 passes
- transactional refs, permissions, structs, arrays, and i31 tests pass

### FW4: Conflicts And SIMD

Exit:

- Waves 6 and 7 pass
- conflict tests and transactional SIMD tests pass

### FW5: Full Proposal Green

Exit:

- all 173 `.wast` files pass or are documented as intentionally diverging from
  Wizard-first semantics
- no unclassified skips remain

## Subagent Dispatch Templates

### Implementer Prompt Shape

Each implementer subagent should receive:

- exact wave name
- exact files in scope
- exact files likely to modify
- relevant design sections from `transactional-wasm-wizard-first.md`
- expected verification command
- explicit instruction not to touch out-of-wave features

Required implementer output:

- status: `DONE`, `DONE_WITH_CONCERNS`, `NEEDS_CONTEXT`, or `BLOCKED`
- files changed
- tests run and results
- remaining blockers

### Spec Reviewer Prompt Shape

The spec reviewer checks:

- only in-wave files/features were changed
- Wizard constants are preserved
- `tmemory` remains separate from ordinary `memory`
- dynamic backend/CC seams are not bypassed
- WAST ledger status is updated

### Code Quality Reviewer Prompt Shape

The code reviewer checks:

- implementation follows existing Wasmtime patterns
- no unrelated refactors
- no ordinary memory lowering reused unsafely for `tmemory`
- tests are focused and deterministic
- errors and traps are explicit

## Dependencies

```text
Milestone 1 foundation
  -> Wave 0 ledger/harness
  -> Wave 1 core control/numerics
  -> Wave 2 memory/bulk memory
Wave 2
  -> Wave 3 tables/elements
Wave 1 + Wave 3
  -> Wave 4 type/binary/import/export
Wave 4
  -> Wave 5 refs/GC objects
Wave 2 + transaction runtime
  -> Wave 6 conflicts
Wave 2 + SIMD parser support
  -> Wave 7 transactional SIMD
Waves 1-7
  -> FW5 full proposal green
```

## Verification Strategy

Start with per-wave commands. Do not rely on the entire WAST suite until the
wave-level command is stable.

Expected shape:

```bash
cargo test --test wast simple-transactions::<wave-filter>
cargo test --test wast tsimd::<wave-filter>
```

Once test names are stable, replace placeholders with exact commands in the
ledger and this roadmap.

Full-suite command:

```bash
cargo test --test wast simple-transactions
cargo test --test wast tsimd
```

The full-suite command is meaningful only after Wave 0 provides filters and
skip accounting.
