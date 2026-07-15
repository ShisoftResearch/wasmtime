# Kotlin TxRef Explicit Persistent Boundary Design

## Problem

Kotlin/WasmGC values can currently be constructed outside a transaction and later
inserted into a transactional persistent object graph. The runtime can promote
those ordinary WasmGC objects into persistent object storage, but the promotion is
implicit. This makes accidental persistence easy and hides the fact that the live
WasmGC object and the eventual persistent object do not share identity.

The project needs an explicit source-level marker for ordinary WasmGC values that
are allowed to enter persistent state, while keeping normal persistent object
types usable inside transactions.

## Goals

- Make reachability the primary persistence invariant: committing a transaction
  persists only objects reachable from committed persistent roots, and discards
  every unreachable transaction-local allocation.
- Define transaction-local object allocation as a universal transactional Wasm
  runtime behavior, independent of Kotlin. Any frontend that emits transactional
  object constructors gets the same isolation and commit behavior.
- Require an explicit Kotlin API before any ordinary WasmGC reference that
  originates outside the transaction can enter a persistent root, persistent
  struct field, or persistent array element.
- Preserve usability by allowing values to be prepared outside a transaction and
  consumed inside a transaction.
- Keep the real graph copy deferred until a persistent write or commit boundary.
- Give Kotlin code a distinct outside-transaction type for persistent-bound
  ordinary references.
- Give the rewriter a copyability capability so only safe value graphs can be
  copied into persistent storage.
- Reject accidental ordinary WasmGC captures during the `twasm-kotlin rewrite`
  step before execution.
- Allow the same explicit reference to be used multiple times, preserving aliasing
  through the existing promotion map.

## Non-Goals

- Do not add a Kotlin compiler plugin in this iteration.
- Do not make `make_txn_ref` perform an immediate clone.
- Do not remove the runtime promotion machinery; it remains the implementation
  mechanism behind explicit references.
- Do not require all persistent class constructors to return a wrapper type inside
  transactions.
- Do not treat Java `java.io.Serializable` or kotlinx serialization as the
  source of truth. The transaction system defines its own copyability capability
  because Kotlin/Wasm persistence is about persistent graph copying, not byte
  serialization.

## Kotlin API

Add this API to the Kotlin transaction SDK. `get()` should remain a normal SDK
function so the rewriter can recognize the call by its stable Kotlin function
name and mark the returned value as explicitly transaction-bound.

```kotlin
class TxRef<T> internal constructor(val value: T)

@Target(AnnotationTarget.CLASS)
@Retention(AnnotationRetention.BINARY)
annotation class TxCopyable

fun <T> make_txn_ref(value: T): TxRef<T> =
    TxRef(value)

fun <T> TxRef<T>.get(): T =
    value
```

`make_txn_ref` means that the wrapped ordinary WasmGC value may be promoted into
persistent state later. It does not clone immediately. `TxRef.get()` is the
explicit boundary operation used inside a transaction.

`@TxCopyable` marks user-defined DTO classes that do not have persistent
identity but whose reachable value graph may be copied into persistent storage.
`@Persistent` implies `TxCopyable`: persistent classes are identity-bearing
persistent types, but their values are also valid copyable payloads when they
cross an explicit `TxRef` boundary.

`TxRef<T>` is reusable. Multiple calls to `get()` from the same `TxRef` are valid.
If the same underlying ordinary object graph enters a transaction more than once,
runtime promotion should map it to one persistent object identity for that
transaction.

Example:

```kotlin
val note = make_txn_ref("this is a note")

transaction {
    val bank = Bank(Account(1_000), Account(200), note.get())
    setRoot("bank", bank)
}
```

## Copyability Rules

`TxCopyable` answers whether a value graph is safe to copy into persistent
storage. It is separate from `TxRef`, which answers whether the user explicitly
allowed an outside value to cross into a transaction.

Copyable types:

- Every `@Persistent` class.
- Every user-defined `@TxCopyable` class.
- Kotlin primitive/value-like types and boxes, including `Boolean`, numeric
  types, `Char`, and unit-like values.
- `String`.
- Pure data containers such as arrays, lists, sets, and maps when their element,
  key, and value types are also copyable.
- Kotlin runtime backing structs that are required to represent the copyable
  built-ins above.

Non-copyable types:

- Files, streams, sockets, and OS handles.
- Coroutine, continuation, thread, process, environment, or scheduler state.
- Arbitrary `externref` values and host resources.
- Unknown Kotlin runtime objects unless the rewriter recognizes them as part of
  an allowed copyable built-in shape or the sidecar explicitly marks them.
- Any type matched by the sidecar deny list. The deny list wins over copyability.

Structural validation:

- A `@TxCopyable` class is valid only if all of its fields are copyable.
- A `@TxCopyable` class may contain references to `@Persistent` objects. The DTO
  itself is copied as a value graph, while the persistent references inside it
  are recorded as references to persistent identity.
- Generic containers are copyable only when their reachable elements are
  copyable. If the rewriter cannot prove this from metadata and recognized
  Kotlin runtime shapes, it must reject the boundary.
- Cycles and repeated references inside a copied graph are allowed when the
  runtime promotion map can preserve them.

## Rewriter Enforcement

The `twasm-kotlin rewrite` pass is the compile-time gate for this iteration. It
must reject ordinary WasmGC references that enter persistent state without
`TxRef.get()` provenance. It must also reject values whose type graph is not
copyable.

Allowed reference origins:

- `TxRef<T>.get()` when `T` is copyable.
- Persistent roots and persistent field or array reads.
- Transaction-local constructors for copyable classes, after their fields are
  checked for copyability and allowed provenance.
- Null references.
- Scalar values are unaffected.

Rejected reference origins:

- String, list, map, DTO, or persistent-class values captured outside a
  transaction and inserted directly into persistent state without `TxRef.get()`.
- Results from unknown ordinary functions when passed into persistent state.
- Ordinary WasmGC values flowing through locals or parameters when the rewriter
  cannot prove an allowed origin.
- Any value with non-copyable reachable state, even when it is wrapped in
  `TxRef`.

The rewriter should track reference provenance while rewriting transaction
functions and persistent accessor functions:

```text
PersistentRef     value is already persistent or was read from persistent state
TxnRefAllowed     value came from TxRef.get()
TxnLocalCopyable  value was constructed inside the transaction and is copyable
OrdinaryRef       value came from normal WasmGC construction, call, local, or parameter
UnknownRef        value cannot be proven safe by local analysis
```

At each persistent boundary, ref-typed values must be `PersistentRef`,
`TxnRefAllowed`, `TxnLocalCopyable`, or null, and the value type must be
copyable. `OrdinaryRef` and `UnknownRef` produce a rewrite error. The error
should name the boundary when possible, for example:

```text
ordinary WasmGC reference enters persistent field Bank.note without make_txn_ref
```

Generic `gcWasm.capture = "allModuleGcTypes"` types are not automatically
user-facing persistent boundaries. Captured Kotlin runtime types such as the
implementation structs behind `String` can be copyable support types without
requiring every internal runtime edge to be written with `TxRef.get()`. The
explicit sidecar persistent and copyable types define the source-level
type-checking boundary.

## Runtime Behavior

Runtime allocation semantics are not Kotlin-specific. Ordinary transactional
Wasm object constructors such as `tstruct.new`, `tstruct.new_default`,
`tarray.new`, `tarray.new_default`, `tarray.new_fixed`, `tarray.new_data`, and
`tarray.new_elem` allocate into a transaction-local object table while a
transaction is active. These allocations do not enter the WasmGC heap or the
shared object table merely because they were constructed.

Reachability is the primary persistence rule. Before commit, the runtime traces
from the transaction's committed persistent roots, including `tglobal`,
persistent table/root entries, and references written into already-persistent
objects. A reachable transaction-local graph is promoted once into persistent
storage. Every transaction-local object outside that graph is discarded. An
abort discards the entire transaction-local table.

Ordinary Wasm globals, tables, locals, and function results are not persistence
roots. A transaction object reference that must escape the transaction must be
published through a persistent root before commit. Committing must not preserve
an object solely because a transaction handle still exists or because the
transaction completed successfully.

The static persistent constructor path is reserved for initialization and static
storage outside active transaction allocation. It does not override
reachability. If such a constructor is used while a transaction is active, its
object must follow the same transaction-local allocation and reachability rules.

Transaction-local object ids and transaction object handles are runtime
implementation details. Reads, writes, casts, array operations, globals, roots,
tables, and promotion must resolve handles through the active transaction state
first and then fall back to the shared object table.

Transaction handle allocation exclusion is monotonic for an `ObjectTable`
lifetime. An unpromoted or expired handle remains in
`reserved_transaction_ref_handles` as a tombstone until full `ObjectTable`
reset. A promoted handle leaves that set only when it is installed in the
permanent handle mapping, and that permanent mapping continues to prevent
reuse. Neither case makes a handle a persistence root.

At commit, promotion copies only the reachable transaction-local graph into
persistent object records. The promotion map preserves cycles and repeated
references, so multiple paths to one local object become multiple paths to one
persistent `ObjectId`. After root publication, the runtime discards the
transaction-local table and all unpromoted handle mappings. If a local object
was promoted, any handle associated with that object may be rebound to the
promoted persistent `ObjectId` so committed `tglobal`, persistent-root, and
persistent-table values remain immediately usable. The handle survives because
the object is persistently reachable, never because the handle itself is a
root. This pre-LP finalization step does not change durable commit atomicity. It
must not first publish every local slot into the shared volatile object table.

The runtime promotion path remains responsible for the actual graph copy. The
runtime does not check whether a live GC reference came from `TxRef.get()`;
explicitness is enforced only by the rewriter. Once a live GC reference reaches a
persistent runtime boundary, the runtime handles it according to its normal
promotion rules.

No immediate clone occurs at `make_txn_ref` or `TxRef.get()`. Copying happens
when an accepted value actually crosses into persistent storage. This can happen
at commit for ordinary object graphs that are reachable from a persistent root,
or earlier inside a transaction when a persistent reference slot is written or
staged.

All runtime persistent-reference write boundaries must treat an ordinary live
WasmGC reference as a promotion source. This includes `tstruct.set`,
`tarray.set`, `tglobal.set`, root publication, table writes if persistent tables
are supported, and any other path that stores or stages a tref into persistent
state.

```text
persistent_ref_slot = live_ref
  null or i31              -> store directly as the current ref leaf
  transaction object handle -> store the referenced ObjectId
  existing live bridge     -> store or promote the bridged ObjectId as needed
  ordinary WasmGC ref      -> promote the ordinary graph, then store the promoted ObjectId
```

Promotion must reuse the existing transaction promotion maps. In particular,
before promoting an ordinary WasmGC reference, the runtime must check whether the
same raw WasmGC reference already appears in `TransactionState.promoted_gc_refs`.
If it does, the write stores the existing promoted `ObjectId`. If it does not,
the runtime promotes the graph, records the raw WasmGC reference in
`promoted_gc_refs`, and stores the new promoted `ObjectId`. This preserves
aliasing when the same `TxRef` or transaction-local object graph is written into
multiple persistent fields, array elements, globals, roots, tables, or other
persistent reference slots.

For a `@TxCopyable` DTO, the DTO graph is copied as value data. For references to
`@Persistent` objects inside that DTO graph, promotion records references to the
persistent identity rather than inventing DTO identity for the referenced object.

## Reference Kotlin Example

The bank example should be updated to show the intended usage:

```kotlin
import twasm.Persistent
import twasm.TxnFunc
import twasm.get
import twasm.make_txn_ref
import twasm.root
import twasm.setRoot
import twasm.transaction

@Persistent
class Account(var balance: Long)

@Persistent
class Bank(
    var alice: Account,
    var bob: Account,
    var note: String
)

@TxnFunc
fun transfer(bank: Bank, fromAlice: Boolean, amount: Long, note: String) {
    if (fromAlice) {
        bank.alice.balance -= amount
        bank.bob.balance += amount
    } else {
        bank.bob.balance -= amount
        bank.alice.balance += amount
    }
    bank.note = note
}

@TxnFunc
fun installFreshBank(alice: Long, bob: Long, note: String) {
    val bank = Bank(Account(alice), Account(bob), note)
    setRoot("bank", bank)
}

fun main() {
    val note = make_txn_ref("this is a note")
    transaction {
        installFreshBank(1_000, 200, note.get())
        val bank = root<Bank>("bank")
        transfer(bank, true, 100, note.get())
    }
}
```

DTO example:

```kotlin
import twasm.Persistent
import twasm.TxCopyable
import twasm.get
import twasm.make_txn_ref
import twasm.transaction

@Persistent
class Account(var balance: Long)

@TxCopyable
class TransferNote(
    var text: String,
    var source: Account,
    var tags: List<String>
)

@Persistent
class Bank(var note: TransferNote)

fun publishNote(bank: Bank, alice: Account) {
    val note = make_txn_ref(TransferNote("rent", alice, listOf("monthly")))
    transaction {
        bank.note = note.get()
    }
}
```

`TransferNote` is copied when promoted. `TransferNote.source` remains a
reference to the persistent `Account` identity.

The sidecar for this example must include the `Bank.note` field and enable
`gcWasm.capture = "allModuleGcTypes"` so `kotlin.String` and its reachable Kotlin
runtime object shapes are known to the persistent layout system.

## Tests

Add focused tests for the rewriter and runtime path:

- Verify a transaction-local object reachable from a committed persistent root
  is promoted and remains recoverable after commit.
- Verify nested reachable transaction-local objects are promoted once and retain
  aliasing and cycles.
- Verify an unrooted transaction-local object is absent from both shared and
  persistent object storage after commit.
- Verify transaction-local objects referenced only by ordinary globals, tables,
  locals, results, or raw transaction handles do not survive commit.
- Verify abort discards all transaction-local objects without publishing them.
- Accept `make_txn_ref("note")` followed by `note.get()` into `Bank.note`.
- Recover the persistent bank example and verify the string graph is persisted.
- Accept a user-defined `@TxCopyable` DTO with `String`, list, and `@Persistent`
  references inside it.
- Accept using the same `TxRef` twice in one persistent graph and verify recovered
  references alias the same promoted object.
- Accept assigning the same `TxRef.get()` result into two persistent reference
  slots through field, array, global, root, or table write paths, and verify both
  slots store the same promoted `ObjectId`.
- Verify runtime promotion for persistent writes reuses
  `TransactionState.promoted_gc_refs` instead of repeatedly promoting the same
  raw WasmGC reference.
- Reject a direct outside string passed into a persistent field.
- Reject a direct ordinary list or map passed into a persistent field.
- Reject `TxRef.get()` into persistent state when the wrapped DTO reaches a
  denied or stateful system type.
- Reject an unknown ordinary function result passed into a persistent root or
  field.
- Keep existing behavior for persistent objects constructed inside transaction
  functions with checked reference arguments.

## Compatibility

Existing runtime promotion support remains in place. Existing Kotlin examples
that relied on implicit ordinary WasmGC promotion must be updated to use
`make_txn_ref`. This is an intentional source compatibility break for
transactional Kotlin code because it turns accidental persistence into a
rewrite-time error.
