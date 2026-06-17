# Kotlin Object Persistence Implementation Log

## 2026-06-17: Kotlin/WasmGC Shape Discovery

- Built `examples/transaction-kotlin/bank` with Kotlin/Wasm.
- `twasm-kotlin inspect` validates `twasm.kotlin.json`.
- The emitted module contains WasmGC struct/array operations that the Kotlin
  lowerer can target.
- Kotlin object persistence remains object-memory only; unsafe linear memory is
  not part of this wave.

## 2026-06-17: Real Kotlin Transaction Rewrite Gate

- `twasm-kotlin rewrite` now maps real Kotlin function/type/field name-section
  metadata instead of assuming exported functions and ordinal-only GC types.
- The tracked Kotlin bank artifact rewrites into a transaction-enabled Wasm
  module with `transfer` and `installFreshBank` recorded as tfunc metadata.
- Kotlin accessor bodies for persistent fields are lowered to real
  `tstruct.*` operations without making the accessors transaction entrypoints.
- The integration gate parses the rewritten Wasm directly and verifies no
  `twasm*` marker imports remain, tfunc metadata exists, transactional object
  operations exist, and Wasmtime can compile the module with GC and transaction
  features enabled.
- File-backed promotion/recovery is covered by the Kotlin-shaped hand-WAT test.
  Full real-Kotlin execution/recovery is still pending root/setRoot marker
  lowering and a callable entrypoint/export strategy for the generated Kotlin
  module.

## 2026-06-17: Kotlin Object Persistence End-To-End Gate

- `twasm-kotlin rewrite` now lowers the real Kotlin SDK inline `root` and
  `setRoot` marker shape to synthetic transactional globals.
- Functions containing lowered root markers are recorded as transaction
  functions, so the generated Kotlin `_start`/`main` path can start a real
  transaction without changing Kotlin source.
- Root marker lowering supports multiple roots when their persistent types are
  distinct through the current inline helper shape. Same-type roots can now use
  explicit `twasm.root.get`/`twasm.root.set` marker imports whose import field
  names the sidecar root.
- Same-transaction `tglobal.get` of an ordinary Wasmtime GC ref now promotes
  the object graph into persistent `ObjectId` storage and returns a
  transaction-handle-compatible ref for transactional object reads.
- The tracked Kotlin bank artifact executes under Wasmtime/WASI, commits to the
  file-backed transaction log, reopens the log, and verifies recovered Bank and
  Account object records with the post-transfer balances.
- The real Kotlin end-to-end test no longer enables the live-reference
  fallback. It registers durable identities for module-defined functions that
  have Wasmtime `VMFuncRef` slots, then verifies recovered object records
  contain inline durable funcref payloads with the rewritten module fingerprint.
- Remaining object-language work is no longer the Kotlin live-funcref fallback;
  it is the compiler/tooling contract for exposing this durable registration
  outside the test-only `_internal::transaction_persistence` namespace.
- Explicit root marker import calls are lowered and their import declarations
  are stripped from the rewritten module. Function indices in code, exports,
  elements, starts, and transaction-object metadata are remapped so the lowered
  module is self-contained.
