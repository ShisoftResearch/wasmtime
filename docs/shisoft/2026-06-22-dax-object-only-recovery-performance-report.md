# DAX Object-Only Recovery Performance Report

Date: 2026-06-22

Host: `shisoft@192.168.10.74`

Purpose: measure recovery time for a two-region fsdax object-only heap using
one PMEM region per socket.

## Configuration

Command shape:

```sh
WASMTIME_TEST_REAL_PMEM=1 \
WASMTIME_DAX_STRESS_OBJECT_ONLY=1 \
WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm \
WASMTIME_DAX_STRESS_BYTES=256GiB \
cargo test -p wasmtime --release dax_pmem_fsdax_persistent_data_stress --lib -- --ignored --nocapture
```

The object-only split sets the entire per-root byte budget to object durable
log data:

```text
/pmem0: split(obj=262144MiB lin=0MiB head=0MiB)
/pmem1: split(obj=262144MiB lin=0MiB head=0MiB)
```

Recovery used 8 workers per PMEM region, with NUMA-local worker samples:

```text
region_workers=[8,8] total_workers=16
numa=[0,1]
region_worker_nodes=[[0,0,0,0,0,0,0,0,0,0],[1,1,1,1,1,1,1,1,1,1]]
```

## Result

Per-region object population and single-region recovery:

```text
/pmem0:
  objects=2921912 recovered=2921912
  logical=104805MiB encoded=262150MiB
  object_population=279.2MiB/s
  single_region_recovery=3.588s

/pmem1:
  objects=2927214 recovered=2927214
  logical=104803MiB encoded=262144MiB
  object_population=226.8MiB/s
  single_region_recovery=4.019s
```

Combined multi-region recovery:

```text
total_objects=5849126
total_recovered=524294MiB
region_recovery_elapsed=[6.334,6.182]s
region_recovery_max=6.334s
merge_elapsed=0.613s
total_recovery_elapsed=6.956s
total_recovery_mibps=75369.5
```

Full benchmark wall time, including object-log population, was:

```text
2134.59s
```

## Interpretation

The measured recovery phase for 512GiB of encoded object durable-log data took
6.956 seconds end to end across both PMEM regions.

This recovery measurement scans durable log metadata and data-record metadata,
then reads object record headers for every winning object publication. It does
not read every byte of every object payload during recovery. Payload validation
in this stress run is sampled:

```text
object_samples=24 per root
```

The result therefore measures object-log recovery and object-header
classification throughput, not full-payload checksum throughput.
