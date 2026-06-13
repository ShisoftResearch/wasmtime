# Transactional Rust Heap Allocator Design

## Goal

Support native Rust heap types such as `String`, `Vec<T>`, and `Box<T>` in
transactional Rust guests while keeping persistent state backed by transactional
linear memory.

## Chosen Shape

Use a global allocator dispatcher in the guest crate:

```rust
#[global_allocator]
static ALLOC: wasmtime_transaction_sdk::alloc::TxAllocator =
    wasmtime_transaction_sdk::alloc::TxAllocator::new();
```

The allocator routes by SDK transaction state:

- outside `transaction!`: allocate from an ordinary volatile guest heap;
- inside `transaction!`: allocate from the persistent transactional heap.

This keeps native Rust syntax:

```rust
let mut name = String::from("alice");
let mut values = Vec::<i64>::new();
values.push(42);
```

The user does not call a special allocation function for ordinary `String` or
`Vec` construction.

## Persistent Representation

Persistent structs may contain native heap types after the SDK provides
`Persist` implementations for those types:

```rust
#[derive(Persist)]
#[repr(C)]
pub struct Profile {
    pub name: String,
    pub scores: Vec<i64>,
}
```

The struct header is stored in `tmemory`. The internal pointer fields in
`String` and `Vec<T>` point at allocations from the persistent transactional
heap, also addressed in `tmemory`.

Initialization must use placement writes, not `&mut` access to a zeroed root.
A zeroed `Vec` or `String` header is not a valid Rust value. The SDK therefore
needs a root write API that stores a freshly constructed value without dropping
an old value.

## Postpass Requirement

Allocator routing alone is not enough. `String` and `Vec<T>` methods access
their buffers through ordinary pointer fields. The postpass must preserve enough
persistent pointer taint for those buffer accesses to become `tload`/`tstore`.

The first implementation may use a conservative taint rule for transaction
guests: an `i32.load` from a tainted persistent address returns a tainted value.
This makes native pointer fields loaded from persistent headers usable as
persistent addresses. If this proves too broad, later work should replace it
with type-aware pointer-field metadata emitted by `Persist`.

## Allocator Metadata

The first allocator is a bump allocator with persistent cursor storage. It does
not reclaim dropped allocations. This is acceptable for the initial demo and
matches the current append-first persistence direction. Reclamation and durable
free-list recovery remain future work tied to GC/recovery design.

The persistent heap region must not overlap fixed roots used by existing demos.
The first heap uses a high fixed base offset in `tmemory`.

## Transaction Semantics

`transaction!` enters SDK transaction allocation mode before running the body
and exits it afterward. Allocations created during the transaction are written
through transactional memory operations after postpass rewriting. If the Wasm
transaction aborts, staged heap writes are discarded with the rest of the staged
`tmemory` state.

## First Success Case

The first end-to-end test should:

1. build a Rust guest that uses native `String` and `Vec<i64>`;
2. initialize a persistent root with those native heap values;
3. mutate the `String` and `Vec` inside a transaction;
4. recover file-backed `tmemory`;
5. reopen the guest and read back lengths, bytes, and numeric values through
   real guest Wasm exports.

## Known Limits

- The initial allocator is append-only.
- Deallocation is a no-op for persistent allocations.
- Persistent allocator metadata is minimal and may be rebuilt or replaced later.
- This does not implement persistent GC for Wasm GC objects.
