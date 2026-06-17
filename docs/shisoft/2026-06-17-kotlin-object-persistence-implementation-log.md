# Kotlin Object Persistence Implementation Log

## 2026-06-17: Kotlin/WasmGC Shape Discovery

- Built `examples/transaction-kotlin/bank` with Kotlin/Wasm.
- `twasm-kotlin inspect` validates `twasm.kotlin.json`.
- The emitted module contains WasmGC struct/array operations that the Kotlin
  lowerer can target.
- Kotlin object persistence remains object-memory only; unsafe linear memory is
  not part of this wave.
