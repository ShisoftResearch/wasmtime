# Persistent Object GC 9B/9C Implementation Plan

This file is superseded.

Use the current plan:

```text
docs/shisoft/2026-06-14-persistent-object-gc-9b-9c-tombstone-less-plan.md
```

Reason: the original version of this plan treated GC tombstones as transaction
object publications. That design was rejected. A later GC-block tombstone plan
was also rejected because Makalu/Ralloc-style recovery can avoid durable
per-object GC tombstones entirely.

The current plan keeps the useful Wave 9A and 9B direction:

- refactor the Wave 9A marker into reusable object-GC machinery
- implement Wave 9B commit-coupled incremental marking
- implement Wave 9C tombstone-less recovery-time GC
- rebuild volatile object table and allocator state from reachable object-data
  winners
