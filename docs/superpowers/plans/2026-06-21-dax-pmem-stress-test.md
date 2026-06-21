# DAX PMEM Stress Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an ignored fsdax stress test that fills configurable gigabyte-scale persistent object and linear-memory data, then reports runtime and recovery throughput.

**Architecture:** Keep the harness inside `crates/wasmtime/src/runtime/transaction/persist.rs` tests so it can exercise `TxDurableLog::create_dax_pmem_fsdax_for_test`, `DurableRegionLog<DaxPmemBlockRegion>`, `StreamPublisher`, and `TMemory` DAX recovery paths without exporting benchmark-only APIs. The test is gated by `WASMTIME_TEST_REAL_PMEM=1`, requires explicit `WASMTIME_DAX_STRESS_BYTES`, accepts one or more roots via `WASMTIME_DAX_STRESS_DIRS`, and removes generated files unless `WASMTIME_DAX_STRESS_KEEP_FILES=1`.

**Tech Stack:** Rust unit tests, Wasmtime transaction persistence internals, XFS fsdax DAX PMEM mappings, `Instant` metrics.

---

### Task 1: Add the ignored stress harness

**Files:**
- Modify: `crates/wasmtime/src/runtime/transaction/persist.rs`

- [ ] **Step 1: Write the failing ignored test and helpers**

Add a `#[ignore]` test named `dax_pmem_fsdax_persistent_data_stress` under the existing transaction persistence test module. The test must:

```rust
#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires real fsdax PMEM, WASMTIME_TEST_REAL_PMEM=1, and WASMTIME_DAX_STRESS_BYTES"]
fn dax_pmem_fsdax_persistent_data_stress() { ... }
```

It should return early with an explanatory `eprintln!` when the real-PMEM or size env gates are missing.

- [ ] **Step 2: Verify the RED path**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture
```

Expected before implementation: compile failure or missing helper failures from the incomplete test body.

- [ ] **Step 3: Implement the stress configuration**

Implement local test-only helpers:

```rust
struct DaxPmemStressConfig {
    roots: Vec<PathBuf>,
    bytes_per_root: u64,
    seed: u64,
    keep_files: bool,
}
```

Parse:
- `WASMTIME_TEST_REAL_PMEM=1` as the hardware gate.
- `WASMTIME_DAX_STRESS_BYTES` as required bytes per root, accepting plain bytes plus `K`, `KiB`, `M`, `MiB`, `G`, `GiB`, `T`, `TiB`.
- `WASMTIME_DAX_STRESS_DIRS` as comma-separated roots; fallback to `WASMTIME_TEST_DAX_PMEM_DIR`; fallback to `/pmem0/wasmtime-dcpmm`.
- `WASMTIME_DAX_STRESS_SEED`, default `0x5eed_dacc_2026_0621`.
- `WASMTIME_DAX_STRESS_KEEP_FILES=1`, default cleanup.

Compute the 45% object / 45% linear / 10% headroom split during execution from `bytes_per_root`.

- [ ] **Step 4: Implement the object workload**

Use `TxDurableLog::create_dax_pmem_fsdax_for_test`, `log.ensure_type_layout`, and `StreamPublisher::publish_object_publications_before_commit`. Generate heavy-tailed object sizes:

```text
55%: 32B..256B
25%: 256B..2048B
15%: 2KiB..32KiB
4%: 32KiB..512KiB
1%: 512KiB..4MiB
```

Batch object publications until roughly 4MiB per transaction or 256 publications, then commit the batch LP. Track object count, target logical bytes, encoded bytes, elapsed populate time, recovery time, and recovered committed object count.

- [ ] **Step 5: Implement the linear-memory workload**

Use `TransactionConfig::with_dax_pmem_fsdax_path`, `TMemory::new`, `grow_to_pages`, and `commit_range`. Compute the per-root linear target from the 45/45/10 split at execution time, then write deterministic 1MiB chunks until that target is reached. Reopen with `TransactionConfig::with_dax_pmem_existing_fsdax_path`, sample deterministic offsets, and report populate and reopen/sample validation time.

- [ ] **Step 6: Implement metrics output and cleanup**

Print one line per root plus object and linear sub-metrics with MiB/s. Remove generated files after successful validation unless `WASMTIME_DAX_STRESS_KEEP_FILES=1`.

- [ ] **Step 7: Verify local compile/gated behavior**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture
```

Expected without env: PASS with an explanatory skip message.

Run:

```bash
cargo fmt --check
git diff --check
```

Expected: both pass.

### Task 2: Run a small real-fsdax smoke stress

**Files:**
- No code edits expected.

- [ ] **Step 1: Copy the branch to the Optane host**

Use the repo’s existing remote checkout path:

```bash
rsync -a --delete --exclude .git --exclude target ./ shisoft@192.168.10.74:/home/shisoft/wasmtime-dax-pmem-test/
```

- [ ] **Step 2: Run a small PMEM stress on both regions**

Run:

```bash
ssh shisoft@192.168.10.74 'cd /home/shisoft/wasmtime-dax-pmem-test && WASMTIME_TEST_REAL_PMEM=1 WASMTIME_DAX_STRESS_DIRS=/pmem0/wasmtime-dcpmm,/pmem1/wasmtime-dcpmm WASMTIME_DAX_STRESS_BYTES=1GiB cargo test -p wasmtime --lib dax_pmem_fsdax_persistent_data_stress -- --ignored --exact --nocapture'
```

Expected: the test completes, prints object and linear populate/recovery metrics for both regions, and removes generated files.

- [ ] **Step 3: Run targeted regression tests**

Run:

```bash
cargo test -p wasmtime --lib dax_pmem -- --format terse
cargo test -p wasmtime --lib tmemory -- --format terse
```

Expected: all relevant local tests pass or pre-existing ignored tests remain ignored.

## Zero-Copy Recovery Requirement

The stress harness must validate recovered object samples through mapped record
bytes. It must not materialize per-winner record bytes during recovery or build a
`Vec<u8>` for every recovered object winner. A 256GiB object dataset is expected
to keep recovery RSS near metadata scale rather than object-data scale.
