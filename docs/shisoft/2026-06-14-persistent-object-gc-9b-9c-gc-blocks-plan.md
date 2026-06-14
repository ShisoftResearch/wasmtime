# Persistent Object GC 9B/9C GC Blocks Implementation Plan

This file is superseded.

Use the current tombstone-less plan:

```text
docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-tombstone-less-plan.md
```

Reason: after reviewing Makalu/Ralloc-style persistent allocators, the active
design no longer uses durable GC tombstones. GC tombstone blocks would add
runtime persistent metadata writes that recovery-time reachability can avoid.

The current baseline is:

- no durable per-object GC tombstones
- no durable mark bits, line marks, object table entries, free lists, or reclaim
  queues
- rebuild volatile object-table and allocator state from committed object data,
  persistent roots, and type/layout metadata during recovery
- move durable storage reuse into a later block/chunk-retirement checkpoint
  design
