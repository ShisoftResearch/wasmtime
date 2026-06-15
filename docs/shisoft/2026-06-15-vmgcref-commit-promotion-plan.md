# VMGcRef Commit Promotion Plan

Date: 2026-06-15
Status: superseded by
`docs/shisoft/2026-06-15-complete-persistent-promotion-workstreams-plan.md`.

## Supersession Note

The original version of this plan promoted raw `ti31` immediates into standalone
`ObjectPayload::I31` records and added a `TI31` object granule domain. That
direction was rejected during the promotion detour.

Current rule:

- `ti31` is an inline durable value (`ObjectValue::I31`), not an object-table
  payload.
- `tfuncref` is an inline durable value
  (`ObjectValue::FuncRef(DurableFuncIdentity)`), not an implicit function
  object-table allocation.
- `texternref` is an inline durable value
  (`ObjectValue::ExternRef(DurableExternIdentity)`), not an implicit external
  object-table allocation.
- Current persistent object-table payload records are `Struct` and `Array`.
- Only `ObjectValue::Ref(Some(ObjectId))` contributes an `ObjectId` graph edge
  for recovery-time tracing and persistent GC.

## Completed Slice Kept From This Plan

- Commit-time promotion exists for transaction-mirrored volatile `tstruct` and
  `tarray` objects already represented in `ObjectTable`.
- Promotion reserves persistent `ObjectId`s before filling payloads, preserving
  cycles and shared references.
- Promotion rewrites heap-object edges to promoted or already persistent
  `ObjectId`s.
- Promotion records optimistic source-object reads and rolls back allocated
  targets if promotion fails.
- Staged persistent roots and staged persistent object payloads are rewritten
  before durable publication.
- File-backed recovery can rebuild reachable persistent struct/array winners
  into the volatile object table.

## Still Pending

Use the current complete-promotion workstreams plan for implementation details.
The remaining work is:

- ordinary Wasmtime GC heap object promotion adapter
- durable identity encoders for `tfuncref` and `texternref`
- final persistent transactional reference ABI
- broader WAST/end-to-end tests over recovered file-backed storage
- durable block/chunk retirement and reuse
