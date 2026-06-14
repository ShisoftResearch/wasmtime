# Persistent Object GC 9B/9C Implementation Plan

This file is superseded.

Use the current plan:

```text
docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-gc-blocks-plan.md
```

Reason: the original version of this plan treated GC tombstones as transaction
object publications. That design was rejected. GC tombstones are global
collector metadata and must live in dedicated GC tombstone blocks, not in
per-thread user transaction logs.

The current plan keeps the useful Wave 9A and 9B direction:

- refactor the Wave 9A marker into reusable object-GC machinery
- implement Wave 9B commit-coupled incremental marking
- implement Wave 9C durable GC tombstones as fixed-size entries in GC-owned
  metadata blocks
- apply tombstones during recovery after selecting live object-data winners
