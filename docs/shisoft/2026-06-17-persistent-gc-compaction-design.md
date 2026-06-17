# Persistent GC Compaction Design

## Goal

Add a physical copying cleaner for mixed live/dead persistent object-data
chunks. The collector keeps `ObjectId` stable, moves only durable object record
locations, and uses the existing transaction log commit protocol for crash
safety.

## Current Baseline

The current persistent GC is a tombstone-less mark-sweep baseline:

- marking starts from committed persistent roots and follows `ObjectId` edges;
- unreachable persistent objects are removed from the volatile `ObjectTable`;
- whole-dead object-data chunks can be retired and reused with block-generation
  validation;
- mixed chunks containing both live and dead current records are kept active.

That is correct but fragments durable object storage under workloads that update
or replace objects over time.

## Design Choice

The first compaction layer is a GC maintenance transaction.

The cleaner selects mixed object-data chunks, copies each live current record in
those chunks into fresh object-data storage, publishes the copied records as
ordinary `TObjectPub` entries with higher versions, flips LP on the final copied
publication, and retires the old chunks only after the LP is durable.

This reuses the existing recovery invariant:

```text
current persistent object record = highest committed version for ObjectId
```

No object references are rewritten because persistent references contain
`ObjectId`, not physical addresses.

## Object Identity

Compaction is physical, not logical.

- `ObjectId` remains unchanged.
- `kind`, `type_layout_id`, and payload bytes remain unchanged.
- the copied record receives a higher per-object publication version.
- all persistent refs inside copied payloads still point at the same `ObjectId`
  values.

The volatile `ObjectTable` must be synchronized with copied records after the
maintenance transaction commits, otherwise a later user transaction could
publish a duplicate or older version. The runtime-side cleaner therefore updates
the table's current record/version for each copied object in the same GC path
that creates the maintenance publications.

## Crash Safety

The ordering is the same as normal object publication:

1. write copied object-data records;
2. flush copied data;
3. fence;
4. write `TObjectPub` log entries;
5. flush log entries;
6. flip LP on the final entry;
7. flush/fence LP;
8. retire old chunks;
9. flush/fence retired chunk metadata.

Recovery behavior:

- crash before LP: copied records are ignored and old records remain current;
- crash after LP but before retirement: copied records have higher committed
  versions and win;
- crash after retirement: old chunk generation metadata makes old entries stale,
  so copied records win and the old chunk is reusable.

## Chunk Selection

The first policy is deterministic and conservative:

- summarize current recovered winners by object-data chunk;
- a chunk is mixed when it contains at least one reachable current record and at
  least one unreachable or obsolete current record;
- select a mixed chunk when its dead bytes are non-zero and its live bytes fit in
  the maintenance transaction budget;
- copy all reachable current records from each selected chunk.

The first implementation can expose a small explicit budget such as
`max_chunks` and `max_live_bytes`. Automatic heuristics can be added after real
workload measurements.

## Recovery Metadata

No per-object tombstones are introduced. Retired blocks/chunks remain the only
durable reclamation metadata. Recovery discovers committed copied records by
normal log replay and ignores stale records through existing block-generation
checks.

## Scope

This milestone implements mixed-chunk evacuation and retirement through a GC
maintenance transaction. It does not implement:

- line-level Immix reuse inside still-active chunks;
- object-id reuse;
- concurrent/background GC;
- pointer-update barriers, because object refs are `ObjectId`s;
- moving ordinary Wasmtime GC objects.

