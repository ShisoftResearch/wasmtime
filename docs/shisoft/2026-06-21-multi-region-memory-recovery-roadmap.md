# Multi-Region Memory And Parallel Recovery Roadmap

Date: 2026-06-21

## Goal

Add a user-configured `Region` layer beneath one Wasmtime transactional memory
so one memory can span multiple homogeneous backend storage instances, such as
`/pmem0` and `/pmem1`, while preserving direct address access and enabling
NUMA-local parallel recovery.

The hot path rule is strict: once memory is mapped and an object or linear
address exists, ordinary reads and writes use that address directly. Region
selection must be handled by virtual memory layout, allocation policy, and
recovery orchestration, not by per-access software routing.

## Core Model

```text
Wasmtime transactional memory
  -> RegionSet
      -> Region 0: address window + local metadata + backend storage
      -> Region 1: address window + local metadata + backend storage
      -> ...
```

Each `Region` owns:

- a user-configured virtual address window
- one backend storage object, such as one DAX fsdax file or one file-backed
  image
- its own block metadata, allocator metadata, type-layout metadata, and
  descriptors
- optional NUMA placement metadata, including node id and CPU set

All regions in one `RegionSet` must use the same backend kind. A memory may be
all DAX regions or all file-backed regions, but not a mixed DAX/file set.

## Address Access Rule

Normal object and linear-memory access must not branch on the address to find a
region:

```text
read object bytes  -> dereference current object record address
write linear bytes -> write the linear address
flush DAX bytes    -> CLWB over the address range, then SFENCE
flush file bytes   -> backend-specific sync over the address range
```

The address range itself is the location. CPU page tables select the mapped
region because each region is mapped into a distinct virtual address window.

Region lookup is allowed only on management paths:

- allocation
- grow/remap
- recovery
- metadata scan
- GC sweep/compaction
- debugging and test validation

## Placement Rules

### Object Records

`ObjectId` remains stable object identity, not a placement key.

Object placement follows the address of the current committed record:

```text
ObjectId -> current object record address -> mapped region by VA window
```

When a transaction writes an object, copy-on-write allocation chooses a new
record address from the transaction thread's region. After commit, the object
table maps the same `ObjectId` to the new address. This allows objects to move
between regions naturally as updates are performed by threads on different
sockets or by an explicit allocation-balancing policy.

### Linear Memory

Linear memory remains in-place. Placement follows the actual linear address
range being modified. A transaction may update linear bytes mapped in a remote
region.

The undo record is stored in the transaction durable-log region, not
necessarily in the same region as the linear bytes. Recovery uses the undo
record's logical linear address to repair the actual target bytes.

### Transaction Durable Log Region

A transaction stays on one thread, and the thread has one owned region for
durable logging and object COW allocation.

```text
transaction thread/socket -> durable log region
durable log region        -> object COW allocation region
```

All durable data records referenced by a transaction log entry live in the same
region as that log entry. This preserves the current region-local
`data_block`, `data_offset`, and generation validation model without changing
log typing.

## Virtual Memory Mapping Strategy

The runtime should reserve one virtual address window per configured region.
Each backend maps its storage into its assigned window.

For Linux:

1. Reserve a window with `PROT_NONE`.
2. Map the DAX/file storage into that window.
3. Grow by mapping more of the same backend inside the already reserved window.
4. Fail early if a user-specified address window cannot be reserved.

The runtime should avoid moving a region to another address after recovery
configuration is accepted. If the user requests fixed windows, mapping failure
is a configuration error.

## Roadmap

### Wave 0: Document Region Invariants

Purpose: freeze the conceptual contract before changing storage code.

- Define `Region`, `RegionSet`, and homogeneous-backend rules in the research
  docs.
- Document that direct address access is the hot path.
- Document that region lookup is management-path only.
- Document that object placement follows record address, not `ObjectId`.
- Document that linear memory placement follows address, while undo placement
  follows the transaction log region.

Exit criteria:

- The design states clearly that no durable log record typing changes are part
  of this roadmap.
- The design states clearly that cross-backend mixed regions are not allowed in
  one memory.

### Wave 1: Region Configuration And Descriptors

Purpose: introduce user-visible region configuration without changing normal
read/write behavior.

Add a region descriptor shape containing:

```text
region id
backend kind
backend path or storage identity
reserved virtual address base
reserved virtual address length
initial mapped length
maximum length
optional NUMA node
optional CPU set
```

For v1, the full region list is user configuration. Region-local metadata
describes the contents of each region; the global list of regions does not need
to be persisted inside region 0.

Exit criteria:

- A memory can be configured with one region and behaves like the current
  single-backend path.
- Invalid mixed backend kinds are rejected during configuration.
- Overlapping or duplicate virtual windows are rejected during configuration.

### Wave 2: Fixed-Window Mapping For Backends

Purpose: make address ranges the routing layer.

Extend DAX and file-backed mapping creation/opening so callers may provide an
address window. The backend must map into that window or fail.

Exit criteria:

- DAX fsdax can map `/pmem0` and `/pmem1` into two configured windows.
- File-backed storage can map multiple files into configured windows.
- Grow keeps each mapping inside its existing reserved window.
- A chunk or record allocation never crosses a region boundary.

### Wave 3: Region-Local Allocation And Thread-Owned Placement

Purpose: route allocation without adding per-object or per-stream placement
maps.

Add a `RegionSet` allocator layer:

- object COW allocation asks the current transaction's owned region
- transaction durable data/log allocation uses the same owned region
- linear-memory byte access remains direct address access
- linear undo records are allocated in the transaction's owned region

The transaction's owned region is selected from thread/socket ownership. The
first DAX implementation should map `/pmem0` to node 0 and `/pmem1` to node 1.
Later balancing policies may move threads or choose alternate owned regions
before a transaction begins.

Exit criteria:

- Updating an object from a socket-1 transaction can publish the new object
  version in the socket-1 region even if the previous version was in socket 0.
- The same `ObjectId` can recover to whichever region contains its latest
  committed version.
- A transaction that updates remote linear memory stores undo in its own
  durable log region.

### Wave 4: Multi-Region Recovery Merge

Purpose: recover all region-local logs and rebuild one global memory state.

Each region is recovered independently using existing region-local validation:

```text
region N log entry -> local data_block/data_offset -> region N metadata
```

After local recovery, the `RegionSet` merges:

- object winners by `ObjectId` and version
- root records by logical root id and version
- tmemory size winners by memory instance and version
- type-layout registries, rejecting conflicting layouts
- linear undo rollbacks by logical linear address/version

Recovered object records are exposed through their actual mapped addresses or
through mapped sources backed by the owning region. The merge must not copy
record bytes into DRAM.

Exit criteria:

- Recovery can rebuild a global object table from committed object winners
  spread across multiple regions.
- Latest-version selection is deterministic when different regions contain
  older and newer versions of the same `ObjectId`.
- Type-layout conflicts across regions fail recovery with a clear error.

### Wave 5: NUMA-Local Parallel Region Recovery

Purpose: use one recovery lane per region/socket, then reuse existing
intra-region parallel recovery.

For a two-socket DAX setup:

```text
/pmem0 region recovery -> node 0 CPU set -> 8 intra-region workers
/pmem1 region recovery -> node 1 CPU set -> 8 intra-region workers
```

This is a per-region worker budget, not a global cap. On the Optane box with
two PMEM regions, recovery should therefore run 16 recovery workers total:
8 workers scanning `/pmem0` on socket 0 and 8 workers scanning `/pmem1` on
socket 1. The goal is to combine the bandwidth of both PMEM regions while
keeping each worker group local to the socket that owns its region.

General rule:

- create a top-level recovery task per region
- pin each task to the region's configured NUMA CPU set when available
- run existing parallel region recovery inside that task with the per-region
  worker budget
- merge recovered summaries after all region tasks finish

If multiple regions share one NUMA node, the scheduler should share that node's
worker budget across those regions instead of blindly running the full
per-region budget multiple times on the same socket.

Exit criteria:

- On the Optane box, `/pmem0` and `/pmem1` recover concurrently.
- The recovery report prints per-region time, bytes, objects, workers, and
  NUMA node.
- The two-region Optane run reports 8 recovery workers for `/pmem0`, 8
  recovery workers for `/pmem1`, and 16 total recovery workers.
- The total two-region wall time trends toward the slower per-region recovery
  time rather than the sum of both regions.

### Wave 6: Region-Aware Persistent GC And Compaction

Purpose: keep GC compatible with address-based placement and moving object
versions.

GC should trace the global object graph after multi-region recovery, but sweep
and block reuse should stay region-local.

Rules:

- reachability is global by `ObjectId`
- chunk retirement is region-local by recovered record address
- object compaction allocates copied records in the GC transaction's owned
  region unless a later balancing policy requests otherwise
- direct access to installed objects continues to use their record addresses

Exit criteria:

- Persistent GC can retire unreachable chunks in the correct owning region.
- GC copying can move an object to a new region by publishing a new version.
- Region-local metadata remains sufficient for reclaiming region-local blocks.

### Wave 7: Balancing Policies

Purpose: exploit the address-placement model for load and capacity balancing.

Initial policies:

- `ThreadSocketLocal`: allocate object COW records in the current thread's
  socket-local region.
- `CapacityWatermark`: choose another region for new transactions when a region
  crosses a configured fullness threshold.
- `PinnedRegion`: force one runtime or test to use a specific region.

Balancing happens before allocation. It does not rewrite existing addresses on
normal access.

Exit criteria:

- Stress tests can spread new object versions across `/pmem0` and `/pmem1`.
- Balancing decisions are visible in benchmark reports.
- No balancing policy requires per-object region metadata beyond the current
  record address.

## Test And Benchmark Roadmap

### Unit Tests

- region descriptor validation rejects overlap, mixed backends, and impossible
  grow limits
- fixed-window mapping fails when the requested address is unavailable
- object update can migrate an `ObjectId` to a new region through COW
- linear undo for remote linear memory is stored in the transaction region
- recovery merge selects the latest object winner across regions

### Integration Tests

- one-region DAX behaves like the current DAX backend
- two-region DAX recovers objects and linear undo records across `/pmem0` and
  `/pmem1`
- two-region file-backed mode behaves the same as two-region DAX at the log and
  recovery level
- persistent GC retires region-local chunks after multi-region recovery

### Optane Benchmarks

Use the two-socket machine with:

```text
region 0 -> /pmem0 -> NUMA node 0
region 1 -> /pmem1 -> NUMA node 1
```

Record:

- object population throughput
- linear-memory population throughput
- per-region recovery throughput
- total recovery wall time
- worker count per region, with the two-region Optane target fixed at
  8 workers per region and 16 workers total
- CPU affinity or NUMA node used by each recovery task

The first target is to preserve the current single-region recovery throughput
per region while reducing total two-region recovery wall time through parallel
region recovery.

Latest 256GiB Optane result on `192.168.10.74` with 8 workers per region:

- Dataset: `WASMTIME_DAX_STRESS_BYTES=128GiB` for each of `/pmem0` and
  `/pmem1`, 117969MiB recovered total.
- Worker locality: `/pmem0` recovery workers sampled on NUMA node 0 and
  `/pmem1` recovery workers sampled on NUMA node 1.
- After vector-reducing only the cross-region merge: region recovery
  `[1.522,1.539]s`, merge `0.158s`, total `1.698s`, total throughput
  `69471.9MiB/s`.
- After also vector-reducing region-local replay winners: region recovery
  `[1.395,1.381]s`, merge `0.136s`, total `1.531s`, total throughput
  `77037.3MiB/s`.

## Non-Goals

- Do not change durable log record typing.
- Do not persist shard ids in log entries.
- Do not route normal reads and writes through software address checks.
- Do not support mixed DAX/file-backed regions in one memory.
- Do not require an `ObjectId -> region` map.
- Do not implement cross-region commit records in this roadmap.
- Do not implement MVCC as part of this roadmap.

## Open Follow-Up Design Questions

1. Whether user configuration alone is sufficient to reopen a multi-region
   memory, or whether a small persistent RegionSet manifest is needed later for
   safety checks.
2. Whether transaction thread ownership should be enforced by pinning, by
   observation, or by an explicit runtime scheduler contract.
3. How much virtual address space should be reserved per region by default for
   growable DAX and file-backed regions.
4. Whether the first production balancing policy should be purely socket-local
   or include capacity watermarks from the beginning.
