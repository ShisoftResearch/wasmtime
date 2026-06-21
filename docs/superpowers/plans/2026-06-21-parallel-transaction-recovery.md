# Parallel Transaction Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make transactional durable-region recovery use bounded parallelism while preserving deterministic recovered metadata, zero-copy object-record access, and existing recovery semantics.

**Architecture:** Keep physical region discovery sequential for the first implementation slice, because it builds the `DataChunkIndex` and skips variable-size chunks. Parallelize the two independent hot phases: per-stream log replay and per-winner classification. Merge worker outputs serially through the existing `BTreeMap` winner logic so duplicate-version errors, root sorting, and recovered winner order remain deterministic.

**Tech Stack:** Rust `std::thread::scope`, existing `anyhow`-style `Result`, existing `BlockRegionBackendView`, no new dependency. The implementation must preserve mapped-slice recovery for file-backed and DAX/fSDAX backends.

---

## File Structure

- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Add `Sync` to `BlockRegionBackend` so immutable recovery views can be shared across scoped worker threads.
  - Add tests proving file-backed and DAX block-region views are usable through shared references.

- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
  - Add `RecoveryOptions` and `RecoveryParallelism`.
  - Add bounded worker-count helpers.
  - Add `recover_region_with_options(...)` and keep `recover_region(...)` as the default auto-parallel entry point.
  - Split serial stream replay merge from replay execution.
  - Add parallel stream replay using `std::thread::scope`.
  - Split winner classification into per-winner classification plus serial merge.
  - Add parallel winner classification.
  - Add parity tests comparing serial and forced-parallel recovery results.

- Modify `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
  - Keep `recover_file_backed_region_snapshot(...)` and `recover_dax_pmem_region_snapshot(...)` calling the default `recover_region(...)`.

- Modify `crates/wasmtime/src/runtime/transaction/persist.rs`
  - Extend DAX stress output with recovery worker count. Do not parallelize the stress harness itself in the same task as recovery logic.

---

## Task 1: Recovery Options and Serial Parity Harness

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [ ] **Step 1: Add the failing parity test first**

Add this test inside the existing `#[cfg(test)] mod tests` in `recovery.rs`. It intentionally references `recover_region_with_options`, `RecoveryOptions`, and `RecoveryParallelism` before they exist.

```rust
#[test]
fn recovery_options_serial_matches_default_recovery() {
    let mut region = VMemoryBlockRegion::new(16).unwrap();
    let type_layouts = TypeLayoutRegistry::default();
    let stream_a = 1;
    let stream_b = 2;
    let stream_a_cursor = region.stream_cursor(stream_a);
    let stream_b_cursor = region.stream_cursor(stream_b);

    append_publication_record(&mut region, stream_a, stream_a_cursor, 1, 0x1000, 1, 11);
    append_publication_record(&mut region, stream_b, stream_b_cursor, 1, 0x1001, 1, 22);
    append_publication_record(&mut region, stream_a, stream_a_cursor, 2, 0x1000, 2, 33);

    let default = recover_region(&region.view(), type_layouts.clone()).unwrap();
    let serial = recover_region_with_options(
        &region.view(),
        type_layouts,
        RecoveryOptions {
            parallelism: RecoveryParallelism::Serial,
        },
    )
    .unwrap();

    assert_eq!(
        default
            .winners
            .iter()
            .map(|winner| (winner.logical_id, winner.version, winner.data_block, winner.data_offset))
            .collect::<Vec<_>>(),
        serial
            .winners
            .iter()
            .map(|winner| (winner.logical_id, winner.version, winner.data_block, winner.data_offset))
            .collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run the test and verify it fails**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_options_serial_matches_default_recovery -- --exact --nocapture
```

Expected: compile failure because `recover_region_with_options`, `RecoveryOptions`, and `RecoveryParallelism` are not defined.

- [ ] **Step 3: Add recovery options and default entry point delegation**

Near the top of `recovery.rs`, after `PendingTransaction`, add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryParallelism {
    Serial,
    Auto,
    Workers(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryOptions {
    pub(crate) parallelism: RecoveryParallelism,
}

impl Default for RecoveryOptions {
    fn default() -> Self {
        Self {
            parallelism: RecoveryParallelism::Auto,
        }
    }
}

const RECOVERY_MAX_WORKERS: usize = 16;
```

Replace the body of `recover_region(...)` with:

```rust
pub(crate) fn recover_region(
    region: &BlockRegionBackendView<'_>,
    type_layouts: TypeLayoutRegistry,
) -> Result<RecoveredRegion> {
    recover_region_with_options(region, type_layouts, RecoveryOptions::default())
}
```

Move the old `recover_region(...)` implementation into a new function:

```rust
pub(crate) fn recover_region_with_options(
    region: &BlockRegionBackendView<'_>,
    type_layouts: TypeLayoutRegistry,
    options: RecoveryOptions,
) -> Result<RecoveredRegion> {
    let discovered = discover_region(region)?;
    let mut winners = BTreeMap::<u64, RecoveryWinner>::new();
    let mut tmemory_undo_rollbacks = Vec::new();

    for stream in discovered.streams.iter() {
        let replay = replay_stream(region, stream, &discovered.data_chunk_index)?;
        tmemory_undo_rollbacks.extend(replay.tmemory_undo_rollbacks);
        merge_recovery_winners(&mut winners, replay.winners)?;
    }

    let winners = winners.into_values().collect::<Vec<_>>();
    let classified = classify_recovered_winners(region, &discovered.data_chunk_index, &winners)?;

    Ok(RecoveredRegion {
        next_stream_id: discovered
            .streams
            .iter()
            .map(|stream| stream.stream_id)
            .max()
            .unwrap_or(0)
            + 1,
        streams: discovered.streams,
        winners,
        object_winners: classified.object_winners,
        tmemory_size_winners: classified.tmemory_size_winners,
        type_layouts,
        root_object_ids: classified.root_object_ids,
        tmemory_undo_rollbacks,
        mapped_region_source: region.mapped_region_source(),
    })
}
```

Add the merge helper below `recover_region_with_options`:

```rust
fn merge_recovery_winners(
    winners: &mut BTreeMap<u64, RecoveryWinner>,
    updates: impl IntoIterator<Item = RecoveryWinner>,
) -> Result<()> {
    for update in updates {
        match winners.get(&update.logical_id) {
            Some(current) if current.version > update.version => {}
            Some(current)
                if current.version == update.version
                    && current.data_block == update.data_block
                    && current.data_offset == update.data_offset => {}
            Some(current) if current.version == update.version => {
                if let Ok((_, object_id)) = unpack_object_granule_id(update.logical_id) {
                    bail!(
                        "duplicate committed object version {} for object id {}",
                        update.version,
                        object_id
                    );
                }
                bail!(
                    "duplicate committed version {} for logical id {}",
                    update.version,
                    update.logical_id
                );
            }
            _ => {
                winners.insert(update.logical_id, update);
            }
        }
    }
    Ok(())
}
```

At the end of `recover_region_with_options`, temporarily ignore `options`:

```rust
let _ = options;
```

This keeps the first step behavior-preserving.

- [ ] **Step 4: Run the parity test and verify it passes**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_options_serial_matches_default_recovery -- --exact --nocapture
```

Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "refactor: add transaction recovery options"
```

---

## Task 2: Make Recovery Views Shareable Across Scoped Workers

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs`

- [ ] **Step 1: Add failing compile-time Sync tests**

Inside `#[cfg(test)] mod tests` in `block_region.rs`, add:

```rust
fn assert_sync<T: Sync>() {}

#[test]
fn block_region_view_is_sync_for_parallel_recovery() {
    assert_sync::<BlockRegionBackendView<'_>>();
}
```

- [ ] **Step 2: Run the test and verify it fails or compiles only after the bound exists**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::block_region::tests::block_region_view_is_sync_for_parallel_recovery -- --exact --nocapture
```

Expected before implementation: compile failure if `dyn BlockRegionBackend` is not `Sync`.

- [ ] **Step 3: Add `Sync` to the backend trait**

Change the trait declaration:

```rust
pub(crate) trait BlockRegionBackend: Sync {
```

Do not add `Send`; scoped recovery workers only share immutable references and do not move backend ownership.

- [ ] **Step 4: Run the Sync test**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::block_region::tests::block_region_view_is_sync_for_parallel_recovery -- --exact --nocapture
```

Expected: pass.

- [ ] **Step 5: Run backend filters**

Run:

```bash
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib dax_pmem -- --format terse
```

Expected: all non-ignored tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs
git commit -m "refactor: make block region recovery views shareable"
```

---

## Task 3: Bounded Worker Count and Partition Helpers

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [ ] **Step 1: Add failing helper tests**

Add these tests in `recovery.rs`:

```rust
#[test]
fn recovery_worker_count_respects_serial_and_worker_limits() {
    assert_eq!(
        recovery_worker_count(RecoveryOptions { parallelism: RecoveryParallelism::Serial }, 8),
        1
    );
    assert_eq!(
        recovery_worker_count(RecoveryOptions { parallelism: RecoveryParallelism::Workers(0) }, 8),
        1
    );
    assert_eq!(
        recovery_worker_count(RecoveryOptions { parallelism: RecoveryParallelism::Workers(2) }, 8),
        2
    );
    assert_eq!(
        recovery_worker_count(RecoveryOptions { parallelism: RecoveryParallelism::Workers(64) }, 8),
        8
    );
}

#[test]
fn recovery_partitions_are_non_empty_and_cover_all_items() {
    let partitions = recovery_partitions(10, 3);
    assert_eq!(partitions, vec![0..4, 4..8, 8..10]);
}
```

- [ ] **Step 2: Run tests and verify they fail**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_worker_count_respects_serial_and_worker_limits -- --exact --nocapture
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_partitions_are_non_empty_and_cover_all_items -- --exact --nocapture
```

Expected: compile failure because helpers are missing.

- [ ] **Step 3: Add helpers**

Below `RECOVERY_MAX_WORKERS`, add:

```rust
fn recovery_worker_count(options: RecoveryOptions, item_count: usize) -> usize {
    if item_count <= 1 {
        return 1;
    }
    let requested = match options.parallelism {
        RecoveryParallelism::Serial => 1,
        RecoveryParallelism::Auto => std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        RecoveryParallelism::Workers(workers) => workers.max(1),
    };
    requested.min(item_count).min(RECOVERY_MAX_WORKERS).max(1)
}

fn recovery_partitions(item_count: usize, worker_count: usize) -> Vec<core::ops::Range<usize>> {
    let worker_count = worker_count.min(item_count).max(1);
    let chunk_len = item_count.div_ceil(worker_count);
    let mut partitions = Vec::new();
    let mut start = 0usize;
    while start < item_count {
        let end = start.saturating_add(chunk_len).min(item_count);
        partitions.push(start..end);
        start = end;
    }
    partitions
}
```

- [ ] **Step 4: Run helper tests**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_worker_count_respects_serial_and_worker_limits -- --exact --nocapture
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::recovery_partitions_are_non_empty_and_cover_all_items -- --exact --nocapture
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "refactor: add bounded recovery worker helpers"
```

---

## Task 4: Parallel Per-Stream Replay

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [ ] **Step 1: Add a serial-vs-parallel replay parity test**

Add this test in `recovery.rs`:

```rust
#[test]
fn parallel_stream_replay_matches_serial_recovery() {
    let mut region = VMemoryBlockRegion::new(48).unwrap();
    let type_layouts = TypeLayoutRegistry::default();
    let stream1 = region.stream_cursor(1);
    let stream2 = region.stream_cursor(2);
    let stream3 = region.stream_cursor(3);

    append_publication_record(&mut region, 1, stream1, 1, 0x1000, 1, 11);
    append_publication_record(&mut region, 2, stream2, 1, 0x1001, 1, 22);
    append_publication_record(&mut region, 3, stream3, 1, 0x1002, 1, 33);
    append_publication_record(&mut region, 1, stream1, 2, 0x1000, 2, 44);
    append_publication_record(&mut region, 2, stream2, 2, 0x1001, 2, 55);

    let serial = recover_region_with_options(
        &region.view(),
        type_layouts.clone(),
        RecoveryOptions {
            parallelism: RecoveryParallelism::Serial,
        },
    )
    .unwrap();
    let parallel = recover_region_with_options(
        &region.view(),
        type_layouts,
        RecoveryOptions {
            parallelism: RecoveryParallelism::Workers(4),
        },
    )
    .unwrap();

    assert_eq!(
        serial
            .winners
            .iter()
            .map(|winner| (winner.logical_id, winner.version))
            .collect::<Vec<_>>(),
        parallel
            .winners
            .iter()
            .map(|winner| (winner.logical_id, winner.version))
            .collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run the test before implementation**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::parallel_stream_replay_matches_serial_recovery -- --exact --nocapture
```

Expected before implementation: it may pass because `Workers(4)` is not used yet. Keep this test as a parity guard and add the worker-count assertion in the next step to force use of the parallel path.

- [ ] **Step 3: Add replay execution helpers**

Add this helper below `merge_recovery_winners`:

```rust
fn replay_streams(
    region: &BlockRegionBackendView<'_>,
    streams: &[RecoveredStream],
    data_chunk_index: &DataChunkIndex,
    options: RecoveryOptions,
) -> Result<Vec<StreamReplay>> {
    let worker_count = recovery_worker_count(options, streams.len());
    if worker_count == 1 {
        return streams
            .iter()
            .map(|stream| replay_stream(region, stream, data_chunk_index))
            .collect();
    }
    replay_streams_parallel(region, streams, data_chunk_index, worker_count)
}

fn replay_streams_parallel(
    region: &BlockRegionBackendView<'_>,
    streams: &[RecoveredStream],
    data_chunk_index: &DataChunkIndex,
    worker_count: usize,
) -> Result<Vec<StreamReplay>> {
    let mut indexed = std::thread::scope(|scope| -> Result<Vec<(usize, StreamReplay)>> {
        let mut handles = Vec::new();
        for range in recovery_partitions(streams.len(), worker_count) {
            handles.push(scope.spawn(move || -> Result<Vec<(usize, StreamReplay)>> {
                let mut out = Vec::new();
                for index in range {
                    out.push((index, replay_stream(region, &streams[index], data_chunk_index)?));
                }
                Ok(out)
            }));
        }

        let mut out = Vec::new();
        for handle in handles {
            let mut batch = handle
                .join()
                .map_err(|_| anyhow!("parallel recovery stream replay worker panicked"))??;
            out.append(&mut batch);
        }
        Ok(out)
    })?;

    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, replay)| replay).collect())
}
```

Update `recover_region_with_options(...)` to replace the direct `for stream` loop with:

```rust
let replays = replay_streams(region, &discovered.streams, &discovered.data_chunk_index, options)?;
for replay in replays {
    tmemory_undo_rollbacks.extend(replay.tmemory_undo_rollbacks);
    merge_recovery_winners(&mut winners, replay.winners)?;
}
```

- [ ] **Step 4: Run stream parity test**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::parallel_stream_replay_matches_serial_recovery -- --exact --nocapture
```

Expected: pass.

- [ ] **Step 5: Run duplicate-version tests**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery:: -- --format terse
```

Expected: all recovery tests pass. Duplicate committed object/version errors must remain unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "feat: replay transaction recovery streams in parallel"
```

---

## Task 5: Parallel Winner Classification

**Files:**
- Modify: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`
- Test: `crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs`

- [ ] **Step 1: Add a serial-vs-parallel classification parity test with objects, roots, and tmemory size**

Add these helpers and this test in `recovery.rs`:

```rust
fn append_committed_raw_publication(
    region: &mut VMemoryBlockRegion,
    stream_id: u32,
    stream: StreamCursor,
    block_seq: u32,
    logical_id: u64,
    version: u32,
    kind: u16,
    type_layout_id: u32,
    payload: &[u8],
) {
    let record =
        TMemory::encode_publication_data_record(logical_id, version, kind, type_layout_id, payload)
            .unwrap();
    let location = region.append_data_record(stream, &record).unwrap();
    let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
    let entry = TMemory::publication_log_entry(
        logical_id,
        version,
        stream_id << 1,
        location.data_block,
        location.data_offset,
        0,
        true,
    )
    .unwrap();
    write_log_entries(region, log_block, &[entry]);
}

fn encoded_empty_struct_record(object_id: u64, version: u32, type_layout_id: u32) -> Vec<u8> {
    let header = TxObjectHeader {
        record_len: u64::try_from(size_of::<TxObjectHeader>()).unwrap(),
        object_id,
        version,
        kind: ObjectKind::Struct as u16,
        flags: 0,
        type_layout_id,
    };
    header.as_bytes().to_vec()
}

fn root_logical_id(domain: PackedGranuleDomain, index: u64) -> u64 {
    ((domain as u64) << 60) | index
}

#[test]
fn parallel_winner_classification_matches_serial_recovery() {
    let mut region = VMemoryBlockRegion::new(64).unwrap();
    let mut type_layouts = TypeLayoutRegistry::default();
    let layout = PersistentTypeLayout::Struct {
        id: TypeLayoutId::DEFAULT_STRUCT,
        fingerprint: 0x7061_7261_6c6c_0001,
        body_size: 0,
        fields: Vec::new(),
    };
    type_layouts.insert(layout).unwrap();

    let stream1 = region.stream_cursor(1);
    let stream2 = region.stream_cursor(2);
    let stream3 = region.stream_cursor(3);
    let stream4 = region.stream_cursor(4);
    let first_object_record =
        encoded_empty_struct_record(41, 1, TypeLayoutId::DEFAULT_STRUCT.get());
    let second_object_record =
        encoded_empty_struct_record(42, 1, TypeLayoutId::DEFAULT_STRUCT.get());
    let root_payload = (41u64 + 1).to_le_bytes();
    let new_pages = 12u64.to_le_bytes();

    append_committed_raw_publication(
        &mut region,
        1,
        stream1,
        1,
        pack_object_granule_id(PackedGranuleDomain::TStruct, 41).unwrap(),
        1,
        PackedGranuleDomain::TStruct as u16,
        TypeLayoutId::DEFAULT_STRUCT.get(),
        &first_object_record,
    );
    append_committed_raw_publication(
        &mut region,
        2,
        stream2,
        1,
        pack_object_granule_id(PackedGranuleDomain::TStruct, 42).unwrap(),
        1,
        PackedGranuleDomain::TStruct as u16,
        TypeLayoutId::DEFAULT_STRUCT.get(),
        &second_object_record,
    );
    append_committed_raw_publication(
        &mut region,
        3,
        stream3,
        1,
        root_logical_id(PackedGranuleDomain::TGlobal, 0),
        1,
        PackedGranuleDomain::TGlobal as u16,
        0,
        &root_payload,
    );
    append_committed_raw_publication(
        &mut region,
        4,
        stream4,
        1,
        pack_tmemory_size_logical_id(None, 0).unwrap(),
        1,
        PackedGranuleDomain::TMemorySize as u16,
        0,
        &new_pages,
    );

    let serial = recover_region_with_options(
        &region.view(),
        type_layouts.clone(),
        RecoveryOptions {
            parallelism: RecoveryParallelism::Serial,
        },
    )
    .unwrap();
    let parallel = recover_region_with_options(
        &region.view(),
        type_layouts,
        RecoveryOptions {
            parallelism: RecoveryParallelism::Workers(4),
        },
    )
    .unwrap();

    assert_eq!(serial.object_winners, parallel.object_winners);
    assert_eq!(serial.root_object_ids, parallel.root_object_ids);
    assert_eq!(serial.tmemory_size_winners, parallel.tmemory_size_winners);
}
```

- [ ] **Step 2: Run the test before implementation**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::parallel_winner_classification_matches_serial_recovery -- --exact --nocapture
```

Expected: compile failure if helper functions are missing, or pass before parallel classification. Either result is acceptable at this step; the next step adds the production split.

- [ ] **Step 3: Split single-winner classification result**

Add this enum near `RecoveredClassifiedWinners`:

```rust
#[derive(Debug)]
enum RecoveredWinnerClassification {
    Object(RecoveredObjectWinner),
    TMemorySize(RecoveredTMemorySizeWinner),
    Roots(Vec<u64>),
    Ignored,
}
```

Add this helper:

```rust
fn classify_recovered_winner(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winner: &RecoveryWinner,
) -> Result<RecoveredWinnerClassification> {
    if let Ok((domain, object_id)) = unpack_object_granule_id(winner.logical_id) {
        return Ok(RecoveredWinnerClassification::Object(classify_object_winner(
            region,
            data_chunk_index,
            winner,
            domain,
            object_id,
        )?));
    }

    let Ok(domain) = packed_granule_domain(winner.logical_id) else {
        return Ok(RecoveredWinnerClassification::Ignored);
    };

    match domain {
        PackedGranuleDomain::TMemorySize => Ok(RecoveredWinnerClassification::TMemorySize(
            classify_tmemory_size_winner(region, data_chunk_index, winner)?,
        )),
        PackedGranuleDomain::TGlobal | PackedGranuleDomain::TTable => {
            Ok(RecoveredWinnerClassification::Roots(classify_root_object_ids(
                region,
                data_chunk_index,
                winner,
                domain,
            )?))
        }
        _ => Ok(RecoveredWinnerClassification::Ignored),
    }
}
```

Add a merge helper:

```rust
fn merge_recovered_classification(
    out: &mut RecoveredClassifiedWinners,
    item: RecoveredWinnerClassification,
) -> Result<()> {
    match item {
        RecoveredWinnerClassification::Object(candidate) => {
            match out.object_winners_by_id_mut().get(&candidate.object_id) {
                Some(current) if current.version > candidate.version => {}
                Some(current) if current.version == candidate.version => {
                    bail!(
                        "duplicate committed object version {} for object id {}",
                        candidate.version,
                        candidate.object_id
                    );
                }
                _ => {
                    out.object_winners_by_id_mut().insert(candidate.object_id, candidate);
                }
            }
        }
        RecoveredWinnerClassification::TMemorySize(winner) => {
            out.tmemory_size_winners.push(winner);
        }
        RecoveredWinnerClassification::Roots(mut roots) => {
            out.root_object_ids.append(&mut roots);
        }
        RecoveredWinnerClassification::Ignored => {}
    }
    Ok(())
}
```

To support this helper, change `RecoveredClassifiedWinners` while classification is being built:

```rust
#[derive(Debug, Default)]
struct RecoveredClassifiedWinners {
    object_winners: Vec<RecoveredObjectWinner>,
    tmemory_size_winners: Vec<RecoveredTMemorySizeWinner>,
    root_object_ids: Vec<u64>,
}
```

If a mutable map is cleaner, use a local `BTreeMap<u64, RecoveredObjectWinner>` in `merge_classified_winners(...)` instead of adding `object_winners_by_id_mut()`. Keep the externally returned struct unchanged.

- [ ] **Step 4: Add parallel classification helper**

Replace `classify_recovered_winners(...)` with a dispatcher:

```rust
fn classify_recovered_winners(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
    options: RecoveryOptions,
) -> Result<RecoveredClassifiedWinners> {
    let worker_count = recovery_worker_count(options, winners.len());
    let classifications = if worker_count == 1 {
        winners
            .iter()
            .map(|winner| classify_recovered_winner(region, data_chunk_index, winner))
            .collect::<Result<Vec<_>>>()?
    } else {
        classify_recovered_winners_parallel(region, data_chunk_index, winners, worker_count)?
    };
    merge_classified_winners(classifications)
}
```

Add:

```rust
fn classify_recovered_winners_parallel(
    region: &BlockRegionBackendView<'_>,
    data_chunk_index: &DataChunkIndex,
    winners: &[RecoveryWinner],
    worker_count: usize,
) -> Result<Vec<RecoveredWinnerClassification>> {
    let mut indexed = std::thread::scope(|scope| -> Result<Vec<(usize, RecoveredWinnerClassification)>> {
        let mut handles = Vec::new();
        for range in recovery_partitions(winners.len(), worker_count) {
            handles.push(scope.spawn(move || -> Result<Vec<(usize, RecoveredWinnerClassification)>> {
                let mut out = Vec::new();
                for index in range {
                    out.push((
                        index,
                        classify_recovered_winner(region, data_chunk_index, &winners[index])?,
                    ));
                }
                Ok(out)
            }));
        }

        let mut out = Vec::new();
        for handle in handles {
            let mut batch = handle
                .join()
                .map_err(|_| anyhow!("parallel recovery winner classification worker panicked"))??;
            out.append(&mut batch);
        }
        Ok(out)
    })?;

    indexed.sort_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, item)| item).collect())
}
```

Add:

```rust
fn merge_classified_winners(
    classifications: Vec<RecoveredWinnerClassification>,
) -> Result<RecoveredClassifiedWinners> {
    let mut object_winners = BTreeMap::<u64, RecoveredObjectWinner>::new();
    let mut tmemory_size_winners = Vec::new();
    let mut root_object_ids = Vec::new();

    for item in classifications {
        match item {
            RecoveredWinnerClassification::Object(candidate) => {
                match object_winners.get(&candidate.object_id) {
                    Some(current) if current.version > candidate.version => {}
                    Some(current) if current.version == candidate.version => {
                        bail!(
                            "duplicate committed object version {} for object id {}",
                            candidate.version,
                            candidate.object_id
                        );
                    }
                    _ => {
                        object_winners.insert(candidate.object_id, candidate);
                    }
                }
            }
            RecoveredWinnerClassification::TMemorySize(winner) => {
                tmemory_size_winners.push(winner);
            }
            RecoveredWinnerClassification::Roots(mut roots) => {
                root_object_ids.append(&mut roots);
            }
            RecoveredWinnerClassification::Ignored => {}
        }
    }

    root_object_ids.sort_unstable();
    root_object_ids.dedup();
    Ok(RecoveredClassifiedWinners {
        object_winners: object_winners.into_values().collect(),
        tmemory_size_winners,
        root_object_ids,
    })
}
```

Update `recover_region_with_options(...)`:

```rust
let classified =
    classify_recovered_winners(region, &discovered.data_chunk_index, &winners, options)?;
```

- [ ] **Step 5: Run classification parity test**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery::tests::parallel_winner_classification_matches_serial_recovery -- --exact --nocapture
```

Expected: pass.

- [ ] **Step 6: Run full recovery test filter**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery:: -- --format terse
```

Expected: all recovery tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "feat: classify transaction recovery winners in parallel"
```

---

## Task 6: Preserve Zero-Copy Object Rebuild and Runtime Callers

**Files:**
- Inspect: `crates/wasmtime/src/runtime/transaction/object_table.rs`
- Inspect: `crates/wasmtime/src/runtime/transaction/state.rs`
- Inspect: `crates/wasmtime/src/runtime/store.rs`
- Test: existing mapped recovery tests

- [ ] **Step 1: Run zero-copy guards**

Run:

```bash
rg -n "record_bytes: Vec<u8>|\\.record_bytes|copied_recovered_winner_record_bytes\\(" crates/wasmtime/src/runtime crates/wasmtime/tests
rg -n "install_mapped_persistent_record|rebuild_from_mapped_recovered_object_winners|with_recovered_region_snapshot" crates/wasmtime/src/runtime
```

Expected:
- `copied_recovered_winner_record_bytes` appears only behind `#[cfg(test)]`.
- Runtime rebuild uses `rebuild_from_mapped_recovered_object_winners`.
- Runtime snapshot callbacks still carry `MappedRegionSource`.

- [ ] **Step 2: Run mapped recovery regression tests**

Run:

```bash
cargo test -p wasmtime --lib runtime::transaction::tests::file_backed_object_layout_recovery::recovered_object_table_installs_mapped_records_without_copying_record_bytes -- --exact --nocapture
cargo test -p wasmtime --lib runtime::transaction::persist::tests::file_backed_recovery_handles_many_large_object_records_without_materializing_bytes -- --exact --nocapture
```

Expected: both pass.

- [ ] **Step 3: Verify the only acceptable object winner install path**

Inspect `crates/wasmtime/src/runtime/transaction/object_table.rs` and confirm runtime recovery still installs mapped records through this path:


```rust
rebuilt.rebuild_from_mapped_recovered_object_winners(
    recovered_type_layouts,
    &reachable_winners,
    mapped_source,
)?;
```

Also confirm any copied recovered-winner helpers remain behind `#[cfg(test)]`:

```rust
#[cfg(test)]
fn copied_recovered_winner_record_bytes(
    winner: &crate::runtime::vm::RecoveredObjectWinner,
    source: &dyn MappedRegionSource,
) -> Result<Vec<u8>> {
    let mut record_bytes = Vec::new();
    winner.with_source_record_bytes(source, &mut |bytes| {
        record_bytes.extend_from_slice(bytes);
        Ok(())
    })?;
    Ok(record_bytes)
}
```

Expected: no production object recovery path copies full record bytes into DRAM.

---

## Task 7: Stress Harness Reporting and PMEM Benchmark

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`
- Test: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Add worker count to stress output**

In `ObjectStressStats`, add:

```rust
recovery_workers: usize,
```

Populate it in `run_dax_object_stress(...)`:

```rust
let recovery_workers = crate::runtime::vm::recovery_worker_count_for_test(
    crate::runtime::vm::RecoveryOptions::default(),
    recovered_winners.len(),
);
```

If `recovery_worker_count` is private to `recovery.rs`, add this test-only export:

```rust
#[cfg(test)]
pub(crate) fn recovery_worker_count_for_test(
    options: RecoveryOptions,
    item_count: usize,
) -> usize {
    recovery_worker_count(options, item_count)
}
```

Print it in the stress line:

```rust
"... rec={:.3}s rec_workers={} lin={:.1}MiB/s ..."
```

- [ ] **Step 2: Run stress entrypoint locally**

Run:

```bash
cargo test -p wasmtime --lib runtime::transaction::persist::tests::dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture
```

Expected: pass and print `set WASMTIME_TEST_REAL_PMEM=1 to run this test` when PMEM env vars are absent.

- [ ] **Step 3: Run fresh 1GiB release stress on the Optane host**

Run:

```bash
rsync -a --delete --exclude .git --exclude target ./ shisoft@192.168.10.74:/home/shisoft/wasmtime-dax-pmem-test/
ssh shisoft@192.168.10.74 'cd /home/shisoft/wasmtime-dax-pmem-test && WASMTIME_TEST_REAL_PMEM=1 WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm WASMTIME_DAX_STRESS_BYTES=1GiB cargo test -p wasmtime --release --lib runtime::transaction::persist::tests::dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture'
```

Expected:
- Test passes.
- Output includes `rec_workers=`.
- Recovery time stays correct and recovered object count equals published object count.

- [ ] **Step 4: Run 256GiB release stress when the 1GiB run is clean**

Run:

```bash
ssh shisoft@192.168.10.74 'cd /home/shisoft/wasmtime-dax-pmem-test && WASMTIME_TEST_REAL_PMEM=1 WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm WASMTIME_DAX_STRESS_BYTES=256GiB cargo test -p wasmtime --release --lib runtime::transaction::persist::tests::dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture'
```

Expected:
- Test passes.
- RSS remains bounded; object record bytes are still mapped, not materialized.
- Compare recovery throughput against the previous serial baseline.

- [ ] **Step 5: Clean artifacts and verify PMEM free space**

Run:

```bash
ssh shisoft@192.168.10.74 'find /pmem0/wasmtime-dcpmm /pmem1/wasmtime-dcpmm -maxdepth 1 -type f \( -name "wasmtime-dax-stress-*" -o -name "dax-pmem-stress-*" \) -delete && df -h /pmem0 /pmem1'
```

Expected: no stress files remain and both PMEM mounts are back near baseline free space.

- [ ] **Step 6: Commit**

```bash
git add crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git commit -m "test: report parallel recovery workers in DAX stress"
```

---

## Task 8: Full Verification

**Files:**
- No planned edits.

- [ ] **Step 1: Run formatting and diff checks**

Run:

```bash
rustfmt --check crates/wasmtime/src/runtime/store.rs crates/wasmtime/src/runtime/transaction/mod.rs crates/wasmtime/src/runtime/transaction/object_heap.rs crates/wasmtime/src/runtime/transaction/object_table.rs crates/wasmtime/src/runtime/transaction/persist.rs crates/wasmtime/src/runtime/transaction/state.rs crates/wasmtime/src/runtime/transaction/tests.rs crates/wasmtime/src/runtime/vm.rs crates/wasmtime/src/runtime/vm/libcalls.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs
git diff --check
```

Expected: both commands exit 0.

- [ ] **Step 2: Run targeted tests**

Run:

```bash
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::recovery:: -- --format terse
cargo test -p wasmtime --lib runtime::transaction::tests::file_backed_object_layout_recovery::recovered_object_table_installs_mapped_records_without_copying_record_bytes -- --exact --nocapture
cargo test -p wasmtime --lib runtime::transaction::persist::tests::file_backed_recovery_handles_many_large_object_records_without_materializing_bytes -- --exact --nocapture
cargo test -p wasmtime --lib runtime::vm::memory::tmemory::block_region::tests::dax_pmem_mapped_source_survives_region_grow -- --exact --nocapture
```

Expected: all pass.

- [ ] **Step 3: Run broad filters**

Run:

```bash
cargo test -p wasmtime --lib transaction:: -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
cargo test -p wasmtime --lib dax_pmem -- --format terse
cargo check -p wasmtime --lib
```

Expected:
- Test filters pass.
- `cargo check` passes. Existing unrelated concurrency warnings may remain, but no new recovery warnings should be introduced.

- [ ] **Step 4: Run final zero-copy search guard**

Run:

```bash
rg -n "copy_publication_payload|install_mapped_persistent_record[\\s\\S]*decode_record\\(|validate_recovered_object_samples[\\s\\S]*encode_object_record_for_test" crates/wasmtime/src/runtime
rg -n "record_bytes: Vec<u8>|\\.record_bytes|copied_recovered_winner_record_bytes\\(" crates/wasmtime/src/runtime crates/wasmtime/tests
```

Expected:
- First command has no production matches.
- Second command only shows test/export compatibility paths already known from the zero-copy recovery work.

- [ ] **Step 5: Final commit**

If all tasks were committed separately, skip this. Otherwise commit the remaining implementation:

```bash
git add crates/wasmtime/src/runtime/vm/memory/tmemory/recovery.rs crates/wasmtime/src/runtime/vm/memory/tmemory/block_region.rs crates/wasmtime/src/runtime/transaction/persist.rs
git commit -m "feat: parallelize transaction recovery"
```

## Benchmark Notes

- 2026-06-21 Optane DCPMM release stress, `WASMTIME_DAX_STRESS_BYTES=256GiB`, roots `/pmem0/wasmtime-dcpmm` and `/pmem1/wasmtime-dcpmm`.
- `/pmem0`: recovered `117970MiB` encoded object data in `1.729s` with `rec_workers=16`, about `66.6GiB/s`; `1,315,441` objects published and recovered.
- `/pmem1`: recovered `117965MiB` encoded object data in `1.939s` with `rec_workers=16`, about `59.4GiB/s`; `1,318,723` objects published and recovered.
- Combined recovery throughput across both sequential root runs: `235935MiB` in `3.668s`, about `62.8GiB/s`; `2,634,164` objects recovered.
- The stress test itself reported `ok` and cleaned both PMEM stress directories back to baseline usage. The SSH wrapper exited nonzero only because the shell variable name `status` is read-only in zsh.

---

## Notes and Constraints

- Do not parallelize object-table rebuild in this plan. `ObjectTable` and `ObjectHeap` are mutable single-owner structures and should stay serial until recovered winner metadata is stable.
- Do not parallelize physical region discovery in the first implementation. It skips variable-sized chunks and builds `DataChunkIndex`; a parallel discovery pass is possible later but needs a separate design for range partitioning and chunk-boundary reconciliation.
- Do not add `rayon` to the default Wasmtime runtime path. `rayon` is feature-gated under `parallel-compilation`; using `std::thread::scope` avoids new feature coupling.
- Preserve deterministic merge order. Worker output must be sorted back to stream/winner input order before serial merging.
- Preserve zero-copy object recovery. Parallel workers may read mapped slices, but they must not store recovered object record bytes in `Vec<u8>`.

## Self-Review

- Spec coverage: The plan parallelizes per-stream replay and per-winner classification, keeps zero-copy recovery intact, and includes PMEM stress validation.
- Completeness scan: No incomplete sections remain. Verification-only gates have explicit commands and expected output.
- Type consistency: `RecoveryOptions`, `RecoveryParallelism`, `recover_region_with_options`, `recovery_worker_count`, and `recovery_partitions` are introduced before later tasks use them.
