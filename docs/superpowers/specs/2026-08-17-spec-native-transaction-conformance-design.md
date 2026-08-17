# Spec-Native Transaction Conformance Design

## Goal

Make Wasmtime's transactional implementation conform to the current
simple-transactions proposal at `wasm-persistence` commit
`9a1151b41b21187f11feb4fbc561175f9ea751e0`. The transactional Wasm binaries
accepted, rejected, decoded, and emitted by the Rust toolchain must use the
proposal's current binary format, matching the reference interpreter's test
suite. The Wasmtime transaction WAST gate must have no failures or ignored
transactional proposal fixtures.

## Scope

This work covers both repositories that form the Wasmtime transactional
frontend and runtime:

- `../wasm-tools-transaction`: WAT parsing, name resolution, validation,
  wasmparser decoding, wasm-encoder emission, and binary round trips.
- This Wasmtime checkout: module representation and translation, Cranelift
  lowering, transactional runtime control flow, and the existing GC/runtime
  failures.

The acceptance corpus is every `*.wast` file discovered under
`../wasm-persistence/test/core/simple-transactions` and
`../wasm-persistence/test/core/tsimd`. The behavior covered by
`tconflict-tmemory.py` will be ported to a Wasmtime Rust integration test.

## Constraints and non-goals

- The target is the proposal's current format and semantics. Legacy
  transactional encodings and compatibility translation are deliberately not
  supported.
- WAT syntax support alone is insufficient. Raw binaries must be accepted and
  emitted in the specified format.
- Browser JavaScript API tests are not part of Wasmtime's embedding-interface
  conformance target. Ordinary Wasm tests remain their existing upstream gates.
- Wizard is a useful independent implementation to compare while debugging,
  but `wasm-persistence`'s specification and tests are authoritative.
- This work does not open or comment on Wasmtime pull requests or issues.

## Specification requirements

The implementation must reflect these current proposal rules:

1. `table` and `ttable` are distinct index spaces, as are `memory` and
   `tmemory`, `elem` and `telem`, and `data` and `tdata`.
2. Entity namespace and reference type are independent. Either ordinary or
   transactional table/element entities may carry `ref` or `tref` values.
3. `ttable` uses limits flag `0x40`; transactional table imports and exports
   use external kind `0x41`, while transactional memory and global external
   kinds use `0x42` and `0x43`; element flags use `0x20` for `telem` and
   `0x40` for `tref`.
4. Indirect-call instructions support optional `flags=n`, with the current
   packed flags/table-index binary encoding and instruction-specific defaults.
5. A `tcall_indirect` outside a transaction may select an ordinary table before
   it starts a transaction; all other selection restrictions remain validated.
6. `tblock` and `ttry` preserve labeled control flow, abort handlers, branches,
   returns, and transaction-permission cleanup semantics.
7. `tglobal.get` is permitted in constant expressions when it refers to a
   preceding immutable global.
8. Local state after a transactional abort is intentionally unspecified; the
   runtime may restore or retain locals, but test behavior must not depend on
   either choice.

## Architecture

### Shared typed transaction model

The transactional wasm-tools fork will represent namespace independently from
value/reference type. It must not derive one from the other or store a single
ordinary index plus a transactional-membership bit.

Each textual and binary entity that can be transactional carries an explicit
namespace discriminator. Resolver tables are separate for ordinary and
transactional functions where necessary, and separately for tables, memories,
elements, and data. Instruction immediates retain the appropriate index type
until validation and encoding are complete.

`CallIndirect` and its transactional/tail-call variants carry an explicit,
optional flags value and a namespace-qualified table index. The parser applies
the proposal defaults only when `flags=n` is absent; encoder and decoder share
one packing/unpacking routine for the current format.

### wasm-tools binary and text boundaries

WAT parsing creates the typed model above. Name resolution rejects a
cross-namespace index before binary emission. The encoder emits only current
table/import/element/indirect-call encodings. The decoder accepts current
encodings and rejects old transaction-marker encodings and invalid flag bits.

The same model flows into the Wasmtime parser path, so WAT and raw binary
modules cannot acquire different transaction semantics.

### Wasmtime module translation

`wasmtime-environ` will define distinct index newtypes and module collections
for the transactional table, memory, element, and data namespaces. During
module construction, explicit instance-slot mappings connect each proposal
namespace to the runtime storage needed by the implementation. Transactional
metadata must be derived from decoded entities, not from the present custom
section that marks ordinary indexes as transactional.

Validation and translation consume namespace-qualified operands. An ordinary
operation cannot address a transactional index, and a transactional operation
cannot address an ordinary index unless an indirect-call flag permits the
spec-defined outside-transaction ordinary-table lookup.

### Indirect calls and transactional control flow

Cranelift receives an indirect-call immediate containing the selected namespace
and flags rather than inferring table kind from a reference type or current
transaction state. It validates table selection, performs an allowed ordinary
table lookup before transaction start, and otherwise uses the transactional
table within the active transaction.

Transactional blocks use a real control-frame representation with a success
continuation and an abort continuation. Labeled `tblock`/`ttry` branches and
returns target the appropriate frame. Abort transfers the documented failure
value to the handler, executes the handler instead of discarding it, and
performs transaction cleanup exactly once when the block started the
transaction. No behavior depends on local rollback.

### Runtime repairs

After the frontend passes the affected WAST fixtures through to execution, the
`tarray.wast`, `tstruct.wast`, and `tconflict-tmemory_1.wast` panics are treated
as independent root-cause investigations. Each repair starts with a focused
failing regression test and traces the GC/object lifetime or allocation path
before changing runtime code.

## Error handling

Invalid representation is rejected by the earliest owning layer:

1. wasmparser rejects invalid binary flags, non-current transaction encodings,
   and malformed packed indirect-call immediates.
2. WAT parsing and resolution reject invalid `flags=n` syntax and wrong
   namespaces.
3. Wasmtime validation rejects namespace misuse and illegal table selection.
4. Runtime traps remain runtime traps only for conditions that pass validation.

No late compatibility rewriting or transaction-kind inference is permitted.

## Test strategy

Every production change follows a red-green-refactor cycle:

1. Add a smallest focused test that demonstrates the required behavior.
2. Run it and observe the expected failure against the old implementation.
3. Implement only the required behavior.
4. Re-run the focused test, its crate tests, then the proposal WAST gate.

The test layers are:

- wasm-tools decoder/encoder tests with exact specified byte sequences for
  table/import/element flags and indirect-call packing;
- WAT parser and resolver tests for separate namespaces, orthogonal reference
  kinds, and `flags=n`;
- Wasmtime module-validation and execution tests for invalid namespace use,
  ordinary-table transactional indirect calls, structured abort flow, and
  transactional global initialization;
- direct, unmodified `wasm-persistence` WAST fixtures discovered by the test
  harness; and
- a Rust integration test that covers the conflict behavior now exercised by
  `tconflict-tmemory.py`.

The final transactional command is:

```sh
WASMTIME_TEST_TRANSACTION_WAST=1 \
  cargo test --test wast transaction-proposal -- --test-threads=1
```

It must report zero failures and zero ignored transaction-proposal tests. The
discovered count is intentionally not hard-coded, so new upstream fixtures are
automatically required.

## Delivery milestones

1. Replace legacy transactional entity modeling in wasm-tools and prove exact
   binary/WAT behavior with focused tests.
2. Carry separate namespaces and indirect-call flags through Wasmtime module
   validation, translation, and lowering.
3. Implement labeled transactional block and abort-handler execution.
4. Diagnose and fix the three existing GC/runtime panics with regression tests.
5. Remove proposal-fixture allowlists, enable every relevant fixture, and pass
   the complete transactional suite plus the Rust conflict integration test.
