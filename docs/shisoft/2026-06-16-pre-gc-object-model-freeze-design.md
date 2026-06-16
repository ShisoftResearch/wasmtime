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

The current pre-GC implementation carries transaction object references in raw
`u32` Wasm lanes for compatibility with existing lowering. These values are not
durable object identity and are not `VMGcRef` values. They are transaction-owned
object reference handles that resolve through the store's `ObjectTable` to
full-width `ObjectId` values.

The freeze does not require a full `TRefType` ABI split yet. Durable records
continue to use full-width `PersistentObjectRefRaw`/`ObjectId` encodings, while
live t-prefixed helper boundaries use transient transaction handles.

Required behavior:

- new persistent object references are allocated as `ObjectId`s;
- object payload reference fields store `ObjectId`, not live `VMGcRef`;
- roots and transactional table/global overlays store durable object identity
  for persistent refs;
- normal t-prefixed casts, branch-on-ref, struct/array get/set/fill/copy/init,
  and tracing resolve transaction handles to `ObjectId` before accessing
  persistent state;
- transaction object table slots carry volatile `VMSharedTypeIndex` metadata
  for live casts/tests against transaction handles. This metadata is rebuilt
  from module/runtime type context and is not durable object identity;
- the legacy mixed live-ref resolver may still accept live GC bridge refs only
  for explicitly tagged WAST compatibility and commit-time promotion seams;
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
- `pre_gc_object_recovery_closure` tests cover reachable cycles, nested
  reachability, unreachable committed winners, root replacement, table roots,
  and mixed object/linear-memory reopen;
- transactional object WAST tests still pass;
- no persistent-path helper persists or recovers a live `VMGcRef`/`VMFuncRef`
  as object identity;
- remaining `SHISOFT-TWASM-MOCK` markers identify only explicit compatibility
  fallbacks or future `TRefType`/GC work.

## Implementation Status

- Persistent object records encode graph edges as `ObjectId`.
- T-prefixed constructors, accessors, permission casts, and static
  initializers use transaction-owned object reference handles at the live ABI
  boundary.
- Live Wasmtime `VMGcRef` mappings are named as transaction live-bridge state.
- The mixed live-ref resolver still accepts live bridge values for explicit
  compatibility paths; it is not a durable identity path.
- Transaction WAST result/argument sentinels are accepted only when the
  store-local WAST fallback flag is enabled. This lets proposal helper imports
  shuttle transaction handles through Wasmtime's current ref-typed host
  boundary without treating those handles as real `VMGcRef`s.
- Ordinary GC `ref.test`, `ref.cast`, and branch-on-cast lowering remains
  unchanged outside transaction function context.
- Persistent publication and recovery do not require live GC/function pointer
  identity.
- Durable function/external references are encoded as restart-stable payload
  leaves and require host/store rebind hooks before live use after reopen.
- Remaining live fallbacks are explicit WAST compatibility seams or future
  `TRefType` ABI work.
- The current live helper ABI is a pre-GC handle ABI, not the final typed
  reference ABI. A true `TRefType` split remains future work.
