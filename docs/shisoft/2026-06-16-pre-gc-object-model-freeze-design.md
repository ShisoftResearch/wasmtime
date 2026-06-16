# Pre-GC Object Model Freeze Design

## Goal

Freeze the persistent object model before persistent GC work by removing the
remaining live Wasmtime reference bridges from persistent paths. Persistent
object identity must be `ObjectId`-based at runtime and after recovery. Live
Wasmtime references may still exist as ordinary volatile values and as
commit-time promotion sources, but they must not be durable object identity.

## Scope Correction: Block/Chunk Reclamation

Coarse durable block/chunk reclamation already exists in this branch and is not
part of this freeze's missing-work list. The current storage layer supports:

- block metadata generations;
- stale log-entry rejection through block generation validation;
- whole-dead object-data chunk retirement and reuse;
- committed linear-undo chunk retirement and reuse;
- retry of post-commit linear-undo retirement after cleanup failures.

This freeze leaves that machinery intact. It does not attempt to add the later
storage-reuse workstreams:

- mixed object-data block copying/compaction;
- line-level Immix reuse inside active chunks;
- clean-abort linear-undo retirement after durable rollback completion;
- `ObjectId` reuse.

Those remain GC/storage-management work after the object ABI is stable.

## Frozen Persistent Reference Rule

Persistent object payloads, roots, table entries, globals, and recovered object
table entries must use durable values only:

- `ObjectId` for persistent object graph edges;
- scalar numeric values for primitive fields;
- inline `ti31` values;
- restart-stable durable function identities for `tfuncref`;
- restart-stable durable external identities for `texternref`;
- null for absent references.

`VMGcRef` and `VMFuncRef` are not durable identities. They may appear only in
three places:

1. ordinary non-transactional Wasmtime GC execution;
2. commit-time promotion input while copying a volatile object into persistent
   object storage;
3. explicit, tagged WAST-compatibility fallback seams until the final tref ABI
   replaces them.

Any path that persists an object record, reconstructs a persistent object,
traces persistent references, publishes a transactional object update, or
recovers a persistent root must not depend on live `VMGcRef` or `VMFuncRef`
identity.

## Runtime ABI Shape

The current implementation still carries some transaction object references in
raw `u32` lanes for compatibility with existing lowering. The freeze does not
require a full `TRefType` ABI split yet, but it does require that the raw lane be
interpreted as an encoded persistent object reference once the value is on a
persistent path.

Required behavior:

- new persistent object references are allocated as `ObjectId`s;
- object payload reference fields store `ObjectId`, not live `VMGcRef`;
- roots and transactional table/global overlays store durable object identity
  for persistent refs;
- casts, branch-on-ref, struct/array get/set/fill/copy/init, and tracing resolve
  through `ObjectId`;
- ordinary Wasmtime `VMGcRef` values are promoted before they enter durable
  payloads;
- a non-promotable live function or external reference traps before durable
  publication unless it has a stable durable identity.

## Promotion Boundary

Promotion is the only allowed bridge from volatile Wasmtime references to
persistent objects.

At commit/publish time:

1. inspect a volatile source object through Wasmtime GC/type-layout metadata;
2. allocate or find an `ObjectId` for the persistent copy;
3. encode fields/elements using durable values;
4. append persistent object data;
5. publish the object update through the durable transaction log.

After this point, the persistent object is a forked durable copy. Future
persistent access must not consult the original `VMGcRef`.

## Function And External References

`tfuncref` and `texternref` are scalar leaves, not object-table allocations.
They must not use live pointer identity in durable records.

- `tfuncref` should encode a restart-stable function identity, such as a module
  namespace plus function index or equivalent stable compiled-module identity.
- `texternref` should encode an application-provided durable external identity.
- Live-only function/external fallback maps are allowed only behind explicit
  compatibility tags and must not be treated as persistent recovery support.

## Recovery Invariant

Recovery rebuilds volatile object-table state from durable records. The durable
records must contain enough information to rebuild:

- current object winners by `ObjectId` and version;
- persistent roots;
- type/layout association;
- object graph edges for tracing;
- scalar leaf values, including inline `ti31`, durable function identities, and
  durable external identities.

Recovery must not require live `VMGcRef`, `VMFuncRef`, pointer addresses, or
volatile side maps from the previous process.

## Testing Gate

The freeze is complete when these checks pass:

- persistent object recovery tests reopen file-backed storage and rebuild object
  graph edges without live-ref maps;
- mixed transactions with linear memory and objects still recover correctly;
- transactional object WAST tests still pass;
- no persistent-path helper persists or recovers a live `VMGcRef`/`VMFuncRef`
  as object identity;
- remaining `SHISOFT-TWASM-MOCK` markers identify only explicit compatibility
  fallbacks or future `TRefType`/GC work.

