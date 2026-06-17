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
