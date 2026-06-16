# Wave 8 Stabilization Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Prove the transactional object-model stabilization branch is ready to
start persistent-GC storage reclamation work.

**Architecture:** This wave runs the branch verification gate across Wasmtime,
the local `wasm-tools-transaction` fork, transaction WAST, file-backed
persistence, Rust SDK/tooling, and the WASI preview1 adapter build. The first
full gate found a transactional indirect-call gap, so Wave 8 resumed after the
Wave 8A Cranelift fix that routes `tcall_indirect` through the transactional
table overlay. The final docs record the exact status and remaining GC-only
work.

**Tech Stack:** Rust, Cargo, Wasmtime test harness, transaction WAST harness,
local wasm-tools transaction fork, CI helper scripts.

---

## Task 1: Wasmtime Focused Transaction Gate

**Files:**

- Verify: `crates/wasmtime/src/runtime/transaction.rs`
- Verify: `crates/wasmtime/tests/transaction_persistence.rs`
- Verify: `tests/transaction_persistence_wast.rs`
- Verify: `tests/transaction_rust_simple_transactions.rs`
- Verify: `tests/transaction_rust_toolset.rs`
- Verify: `crates/transaction-sdk/tests/macros.rs`
- Verify: `crates/transaction-tools/tests/*.rs`

- [x] **Step 1.1: Run formatting and whitespace checks**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo fmt --check
git diff --check
```

Expected: both commands exit 0.

- [x] **Step 1.2: Run transaction runtime and persistence tests**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo test -p wasmtime transaction:: --lib -- --format terse
cargo test -p wasmtime --test transaction_persistence -- --format terse
cargo test --test transaction_persistence_wast -- --format terse
```

Expected: all commands exit 0.

- [x] **Step 1.3: Run Rust SDK and transaction toolset tests**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo test -p wasmtime-transaction-sdk --tests -- --format terse
cargo test -p wasmtime-transaction-tools --tests -- --format terse
cargo test --test transaction_rust_simple_transactions -- --format terse
cargo test --test transaction_rust_toolset -- --format terse
```

Expected: all commands exit 0.

- [x] **Step 1.4: Run fuzzing crate check**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo check -p wasmtime-fuzzing --lib
```

Expected: command exits 0.

---

## Task 2: wasm-tools Transaction Fork Gate

**Files:**

- Verify: `/home/shisoft/Code/Research/wasm-tools-transaction`

- [x] **Step 2.1: Run fork formatting and whitespace checks**

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo fmt --check
git diff --check
```

Expected: both commands exit 0.

- [x] **Step 2.2: Run parser/generator checks used by Wasmtime**

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo check -p wasmparser --lib
cargo check -p wasm-smith --all-features --lib
cargo build --bin wasm-tools
```

Expected: all commands exit 0.

---

## Task 3: Full Branch Gate

**Files:**

- Verify: `ci/run-tests.py`
- Verify: `ci/build-wasi-preview1-component-adapter.sh`

- [x] **Step 3.1: Run the full transaction-enabled CI test gate**

```sh
cd /home/shisoft/Code/Research/wasmtime
CARGO_TARGET_DIR=/tmp/wasmtime-wave8-target CARGO_INCREMENTAL=0 \
WASMTIME_TEST_TRANSACTION_WAST=1 CARGO_BUILD_JOBS=2 \
  python3 ./ci/run-tests.py --locked --exclude=wasi-preview1-component-adapter -- --format terse
```

Expected: command exits 0. If this fails for environmental reasons, capture the
exact failing command and error in the implementation log and do not claim the
full gate passed.

- [x] **Step 3.2: Build the WASI preview1 adapter with local wasm-tools**

```sh
cd /home/shisoft/Code/Research/wasmtime
PATH=/home/shisoft/Code/Research/wasm-tools-transaction/target/debug:$PATH \
  ./ci/build-wasi-preview1-component-adapter.sh
```

Expected: command exits 0.

---

## Task 4: Record Stabilization Status

**Files:**

- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`
- Modify:
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`
- Modify:
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md`

- [x] **Step 4.1: Add a Wave 8 status section**

Add a dated section near the top of
`docs/shisoft/transactional-wasm-implementation-log.md` named:

```markdown
## 2026-06-16 Wave 8 Stabilization Gate
```

Record each command from Tasks 1 through 3 with pass/fail status. Do not
summarize a skipped or failed command as passing.

- [x] **Step 4.2: Mark the roadmap Wave 8 checklist**

In
`docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`,
mark each Wave 8 checklist item complete only if the matching command passed.
If any command failed, leave the item unchecked and record the blocker in the
implementation log.

- [x] **Step 4.3: Mark this implementation plan**

In
`docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md`,
mark Wave 8 steps complete only after their commands or doc updates are done.

---

## Task 5: Review And Commit Wave 8

**Files:**

- Review the Wave 8A code fix, Task 4 docs, and this plan file.

- [x] **Step 5.1: Run final lightweight checks**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo fmt --check
git diff --check
```

Expected: both commands exit 0.

- [x] **Step 5.2: Run subagent review gates**

Use subagent-driven development review gates:

- spec review: verify the log and roadmap report the real command outcomes and
  do not overclaim stabilization if any gate failed
- quality review: verify the status entry is concise, auditable, and does not
  duplicate stale older status blocks

- [x] **Step 5.3: Commit Wasmtime Wave 8 changes**

Commit without any `Co-authored-by` annotation:

```sh
cd /home/shisoft/Code/Research/wasmtime
git add docs/shisoft/2026-06-16-wave8-stabilization-gate-plan.md \
  docs/shisoft/2026-06-16-wave8a-tcall-indirect-overlay-plan.md \
  crates/cranelift/src/func_environ.rs \
  crates/cranelift/src/translate/code_translator.rs \
  docs/shisoft/transactional-wasm-implementation-log.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md
git commit -m "Fix transaction indirect calls through table overlays"
```

---

## Self-Review

- Spec coverage: each Wave 8 roadmap item maps to a task above.
- Placeholder scan: no placeholders remain; failed command handling is
  specified explicitly.
- Type consistency: this wave is verification/docs-only and introduces no new
  runtime types.
