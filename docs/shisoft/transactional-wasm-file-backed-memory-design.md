# Transactional Wasm FileBackedMemory Design

Date: 2026-06-09

## Goal

Add `FileBackedMemory` as the filesystem-backed transactional `tmemory`
backend. It is the file-backed analogue of `NVMemory`: transaction writes still
use the existing copy-on-write workspace, but commit publishes bytes into a
shared file mapping and forces them toward durable storage with filesystem
flush operations.

The first implementation is for the research `transaction` branch and stays
behind the default-on `transaction` Cargo feature. Ordinary Wasmtime memory
remains unchanged.

## Relationship To Existing Backends

The three `tmemory` backends should have parallel roles:

- `VMemory`: volatile block/chunk storage, no-op `flush` and `fence`.
- `NVMemory`: PMEM-shaped block/chunk storage, range `CLWB` and `SFENCE`.
- `FileBackedMemory`: file-backed block/chunk storage, range `msync` and
  `fdatasync`/`fsync`.

`FileBackedMemory` should reuse the same `BlockRegionBackend`,
`MappedLinearRegion`, and `TMemoryBackendStorage` shape as `VMemory` and
`NVMemory`. It should not change ordinary Wasmtime linear memory, ordinary GC,
or object-table storage.

## Persistence Model

The filesystem-backed persistence analogy is:

```text
NVMemory commit:
  write mapped bytes
  CLWB dirty cache lines
  SFENCE

FileBackedMemory commit:
  write shared mapped bytes
  msync dirty page range
  fdatasync/fsync backing file
```

`msync` is the closest filesystem-backed analogue to a range `CLWB`: it pushes
dirty pages in the mapped range toward the backing file. `fdatasync` or `fsync`
is the closest analogue to the durability fence: it waits for the file's dirty
data, and sometimes metadata, to reach the storage layer.

This is not the same as PMEM. The operating system, page cache, filesystem, and
storage device can still affect ordering and crash behavior. The first backend
must therefore claim only that committed ranges are flushed through the normal
filesystem durability APIs. It must not claim full power-fail recovery until
durable region headers, commit records, recovery loading, and restart tests
exist.

## Mapping Requirements

The backend must use a shared writable file mapping. The existing Wasmtime
`Mmap::from_file` helper is not sufficient for this backend because the Unix
implementation maps files with `MAP_PRIVATE`; writes through that mapping are
copy-on-write and do not publish to the backing file.

The first implementation should add a transaction-local file mapping helper,
rather than changing ordinary Wasmtime mmap behavior. On Unix this helper should
use:

- `OpenOptions::new().read(true).write(true).create(true)` for explicit files.
- `File::set_len(...)` before mapping or remapping.
- `mmap(..., PROT_READ | PROT_WRITE, MAP_SHARED, file, 0)` for the byte region.
- `msync(..., MS_SYNC)` for `flush(offset, len)`.
- `File::sync_data()` for the default `fence()`.
- `File::sync_all()` after file creation, resize, or metadata-sensitive
  publication.

For non-Unix targets, the first implementation may return a clear unsupported
error unless there is already an appropriate shared writable mmap and flush API
available in Wasmtime's platform layer.

## Configuration

`FileBackedMemory` needs two modes:

1. Research temp-file mode.
   - Creates an internal temporary file.
   - Uses the file-backed path and flush/fence operations.
   - Deletes the file when the store/backend is dropped.
   - Supports normal unit tests without requiring user-provided paths.

2. Explicit path mode.
   - Uses a configured path or directory for persistence experiments.
   - Creates or truncates the file for the first backend wave.
   - Later recovery work will add open-existing validation and region-header
     compatibility checks.

The default `TransactionConfig` remains `VMemory`. Selecting temp-file
`FileBackedMemory` should be explicit, and selecting explicit-path
`FileBackedMemory` should also be explicit. No WAST test should require an
external path by default.

## Commit And Growth Ordering

For each committed `tmemory` byte range:

1. Copy staged transaction bytes into the shared mapped file region.
2. Call `flush(offset, len)` on the file-backed block region. On Unix this is
   `msync(MS_SYNC)` over the affected mapped page range.
3. Call `fence()` on the file-backed block region. The default is
   `File::sync_data()`.
4. Update volatile granule metadata as the current runtime does.

For growth beyond current capacity:

1. Unmap the old region.
2. Extend the file with `set_len(new_capacity)`.
3. Remap the file with `MAP_SHARED`.
4. Flush copied live bytes.
5. Use `sync_all()` after resize because file length is metadata.

The backend can keep granule metadata volatile in this wave, matching the
current `NVMemory` limitation. Durable metadata layout and restart recovery are
future work.

## Testing

Normal tests should cover behavior that does not require crash recovery:

- temp-file `FileBackedMemory` can be constructed
- commits write bytes that are readable through the backend
- flush rejects out-of-bounds ranges
- growth preserves committed bytes
- `TransactionConfig` accepts temp-file mode and explicit-path mode
- unsupported platforms report clear errors

Restart/power-fail tests must be ignored or gated behind an environment
variable, for example:

```text
WASMTIME_TEST_FILE_BACKED_TMEMORY_RECOVERY=1
```

Those tests should remain boundary markers until the backend has durable region
headers and recovery loading.

## Non-Goals

The first `FileBackedMemory` wave does not implement:

- durable transaction logs
- durable object-table recovery
- durable persistent-object heap recovery
- open-existing region validation
- crash-consistent multi-range commit
- directory-entry durability guarantees beyond a best-effort explicit-path
  creation path
- public Wasmtime API stabilization for persistent transaction storage

## Spec Self-Review

- No full persistence claim: the design only claims `msync` plus
  `fdatasync`/`fsync` publication, not restart recovery.
- No conflict with `NVMemory`: `FileBackedMemory` is a separate backend with
  filesystem durability APIs, not a replacement for PMEM.
- No accidental ordinary-memory scope: ordinary Wasmtime memory and GC stay
  unchanged.
- Implementation scope is narrow enough for one follow-up plan: shared mapping
  helper, block region, backend storage, config selection, tests, and docs.
