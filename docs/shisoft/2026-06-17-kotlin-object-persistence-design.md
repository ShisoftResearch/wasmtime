# Kotlin/Wasm Transactional Persistent Object Design

Date: 2026-06-17

## Goal

Add a tracked Kotlin/Wasm customer-language path for transactional persistent
objects on this branch. The first Kotlin target is object memory only:

```text
Kotlin @Persistent class
  -> Kotlin/Wasm WasmGC structs/arrays
  -> transaction Kotlin lowerer
  -> real transactional Wasm object operations
  -> ObjectId-backed persistent object heap
  -> file-backed recovery and persistent GC
```

Kotlin's unsafe linear-memory API is explicitly out of scope for the first
wave. Kotlin still emits ordinary linear memory for runtime scratch data and
interop buffers, but normal Kotlin object graphs should be handled through
WasmGC structs and arrays.

## Repository Shape

The Kotlin work is tracked from the start.

- `examples/transaction-kotlin/bank`: first Kotlin/Wasm example.
- `examples/transaction-kotlin/sdk`: Kotlin source annotations and helper
  functions used by examples.
- `crates/transaction-tools`: extended with a Kotlin lowering mode or
  `twasm-kotlin` binary.
- `tests/transaction_kotlin_*`: integration tests for build, lowering,
  validation, execution, recovery, and persistent GC.

All changes stay behind the existing `transaction` feature policy for this
research branch where they touch Wasmtime runtime behavior. The Kotlin example
and tooling may be tracked unconditionally, but they must not require upstream
users to exercise the transaction feature unless they invoke the transaction
toolchain.

## Kotlin User Surface

The first Kotlin API should look like ordinary Kotlin with annotations and a
transaction block:

```kotlin
@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
}

fun main() {
    transaction {
        val bank = root<Bank>("bank")
        transfer(bank, true, 100)
    }
}
```

The API is intentionally narrow:

- `@Persistent` marks classes and arrays that may be represented as persistent
  objects.
- `@TxnFunc` marks functions whose persistent object access must lower to
  transactional object operations.
- `transaction { ... }` marks the dynamic transaction boundary.
- `root<T>("name")` resolves a durable root to an `ObjectId`-backed persistent
  object.

The first wave supports persistent classes with primitive fields, nullable and
non-null persistent references, and persistent arrays. Kotlin collections,
strings, coroutines, exceptions, reflection, virtual dispatch details, and
unsafe linear memory are out of scope until a later design revision explicitly
adds them.

## No Kotlin Compiler Fork

The first implementation should not fork Kotlin or require Kotlin compiler
changes. The flow is:

```text
Kotlin source
  -> Kotlin/Wasm Gradle build
  -> Kotlin-emitted core Wasm with WasmGC
  -> transaction Kotlin lowerer
  -> transactional Wasm module
  -> transaction Wasmtime
```

The Kotlin SDK helpers compile as normal Kotlin. The lowerer recognizes them
and removes or rewrites them. A future Kotlin compiler plugin can improve
diagnostics and metadata, but it is not part of the first working path.

## Metadata

The lowerer needs stable metadata for persistent class layouts and transactional
functions. Kotlin/Wasm naming and annotation encoding may change, so the first
toolchain should not rely only on incidental annotation strings in the Wasm
binary.

The first source of truth is a compact sidecar metadata file produced or
checked in with the Kotlin example:

- persistent class names
- field names, field kinds, and nullability
- persistent array element kind
- transaction function names
- root names and root types

The lowerer cross-checks sidecar metadata against the Wasm module:

- expected names must resolve to exactly one Wasm function/type where possible
- field counts and field kinds must match the WasmGC type shape
- unresolved or ambiguous names fail the build

`nullable` in the sidecar describes the emitted WasmGC storage type observed by
the lowerer, not Kotlin source-level nullability. Kotlin may emit nullable
storage for fields whose source types are non-null because runtime
initialization and object-layout details are represented below the source type
system. Source-level non-null guarantees can be reintroduced later as an
additional validation layer, but the first lowerer must match the actual WasmGC
layout it rewrites.

Later, a Gradle task or compiler plugin can generate this sidecar from Kotlin
source annotations. The initial checked-in sidecar is acceptable for proving the
runtime and lowering model, but it must be treated as scaffold, not the final
developer experience.

## Lowering Model

The lowerer rewrites Kotlin helper shapes and WasmGC operations into real
transactional Wasm. It must not create a separate Kotlin-only runtime ABI that
bypasses the proposal instruction path.

Required first-wave rewrites:

- `transaction { ... }` helper calls become real transaction entry/exit
  boundaries.
- `@TxnFunc` functions become transactional functions.
- non-transactional calls into `@TxnFunc` functions become transactional calls.
- `root<T>("name")` becomes a durable root lookup that returns a persistent
  object reference encoded through the branch's `ObjectId` ABI.
- Explicit root marker imports may encode stable root names when the inline
  Kotlin helper body does not expose the string operand. The supported ABI is
  `twasm.root.get`/`twasm.root.set` with the import field equal to the sidecar
  root name. The lowerer consumes those calls, strips the marker import
  declarations, and remaps function indices so the rewritten module is
  self-contained.
- persistent `struct.new`/constructor patterns become promotable object
  construction. They may remain ordinary Kotlin/WasmGC objects until they are
  stored into a persistent graph, assigned to a persistent root, or otherwise
  made persistent by the transaction.
- persistent `struct.get`/`struct.set` become transactional object reads and
  writes.
- persistent `array.new`, `array.get`, `array.set`, and `array.len` become
  transactional array allocation/access operations.
- nullable persistent references become durable nullable `ObjectId` references.
- ordinary Kotlin objects outside persistent reachability remain ordinary
  WasmGC objects and are not persisted.

The lowered module should use the same transactional object operation family as
the current simple-transactions WAST tranche: `tfunc`, `tcall`, `tstruct`,
`tarray`, transactional reference/cast operations where needed, and the
existing persistent object runtime paths. SDK marker calls must be removed from
executed code. Explicit marker import declarations are root-name carriers for
the lowerer only; they must not remain in the executed module after rewriting.

## Runtime Semantics

Persistent Kotlin objects use the existing persistent object model:

- persistent identity is `ObjectId`
- durable object records carry object headers, layout/type ids, version, and
  payload
- transaction writes stage object payload updates and publish through the
  durable log/data path
- recovery rebuilds the volatile object table from committed object winners
- persistent GC traces durable roots and `ObjectId` edges
- Kotlin live `VMGcRef` values are not durable identity

`@Persistent` declares a durable layout and promotion eligibility; it does not
mean every instance is immediately allocated in persistent storage. A
transaction may create ordinary Kotlin/WasmGC instances of `@Persistent`
classes, use them as temporary objects, and promote them only if they become
reachable from persistent state before commit.

Promotion happens inside an active transaction. The first wave promotes when a
volatile `@Persistent` object is:

- stored into a persistent object field,
- stored into a persistent array element,
- assigned as a durable root value, or
- reached through another promoted object's persistent fields or array
  elements during promotion closure traversal.

The runtime/lowerer maintains a transaction-local volatile-reference-to-
`ObjectId` map while promoting. This map is not durable object identity and is
discarded when the transaction ends. It exists only to preserve aliasing and
cycles during one transaction: two references to the same volatile object must
publish one persistent `ObjectId`.

Promotion copies the object's current field/array payload into the persistent
object data path and publishes it through the normal transaction commit log.
If a promoted object references another volatile object with persistent layout
metadata, that object is promoted recursively. If it references an ordinary
Kotlin object without persistent layout metadata, the transaction must fail
before commit with a clear unsupported-promotion diagnostic.

Objects created inside a transaction but never made reachable from persistent
state are discarded with the ordinary Kotlin/WasmGC heap and do not produce
durable records.

## Validation Requirements

Validation must prove this is real transactional Wasm, not a Kotlin-specific
shortcut.

The first wave must include these gates:

1. Kotlin/Wasm build gate:
   - `gradle` builds the tracked Kotlin example to Wasm/WASI.

2. Lowerer gate:
   - `twasm-kotlin` or equivalent rewrites the Kotlin module.
   - the report lists transaction functions, persistent object layouts, roots,
     and rewritten object operations.
   - SDK marker calls are absent after lowering.

3. Simple-transactions compatibility gate:
   - the lowered module uses the same transactional instruction family and
     parser/lowering/runtime path as `transaction-proposal/simple-transactions`.
   - tests inspect or disassemble the generated module and require real
     `tfunc`/`tcall`/`tstruct`/`tarray` transactional operations rather than
     private imports.
   - representative generated modules are run through the same Wasmtime
     transaction feature configuration used by the simple-transactions WAST
     suite.
   - the existing simple-transactions WAST suite remains part of the regression
     gate so Kotlin lowering cannot drift from the proposal surface.

4. Execution gate:
   - the lowered Kotlin bank example runs under the transaction Wasmtime
     runtime.
   - a transaction mutates persistent object state.
   - abort/trap leaves persistent state unchanged.

5. Recovery gate:
   - the example commits to file-backed persistent storage.
   - a fresh runtime recovers the object table from durable logs/object records.
   - recovered root and object fields match the committed state.

6. Persistent GC gate:
   - reachable Kotlin persistent objects survive persistent GC.
   - unreachable Kotlin persistent objects become collectible under the current
     mark-sweep/maintenance transaction implementation.

7. Promotion gate:
   - a transaction can create an ordinary Kotlin/WasmGC object whose class has
     persistent layout metadata, store it into a persistent root/object graph,
     commit, recover, and observe it as a durable `ObjectId`-backed object.
   - repeated references to the same volatile object in one transaction promote
     to one `ObjectId`, preserving sharing.
   - unsupported non-persistable referenced objects are rejected before commit.

The validation suite must not modify upstream or proposal WAST files. Any
generated fixtures for Kotlin belong in transaction-specific Rust tests or
tracked Kotlin example test data.

## Non-Goals For The First Wave

- Persistent Kotlin unsafe linear memory.
- Transparent persistence for arbitrary Kotlin standard-library collections.
- Kotlin compiler plugin.
- Full source-level Kotlin diagnostics.
- Browser/Compose support.
- Multi-module Kotlin package metadata.
- Durable identity for arbitrary ordinary WasmGC objects not marked
  `@Persistent`.
- Performance tuning.

## First Milestone

The first milestone is complete when a tracked Kotlin bank example can:

1. build with the local Gradle/Kotlin/Wasm environment,
2. lower into real transactional Wasm object operations,
3. pass simple-transactions compatibility checks,
4. run a successful commit and abort scenario,
5. recover committed object state from file-backed persistent storage, and
6. promote a transaction-local Kotlin object into a durable object graph, and
7. survive a persistent GC pass for reachable bank/account objects.
