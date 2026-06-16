# Wave 7 Type-System Cleanup Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` or `superpowers:executing-plans` to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** Keep the current transaction reference permission metadata honest as
a temporary parser/validator/lowering carrier, while explicitly deferring the
full `TRefType` split until the live persistent reference ABI is stable.

**Architecture:** Runtime permissions remain state attached to `TransactionState`
and `GranuleId`, with persistent objects using `GranuleId::Object(ObjectId)`.
The `wasmparser::RefType.transaction_permission` bits are parser/validator
type-state only; ordinary Wasm refs must continue to decode with
`TransactionRefPermission::None`.

**Tech Stack:** Rust, wasmparser transaction fork, Wasmtime docs,
transaction object-model roadmap.

**Status:** Tasks 1 through 3 are complete as of 2026-06-16. Commit steps in
Task 4 remain intentionally unchecked for controller review and manual commit
handling.

---

## Task 1: Pin The Temporary `RefType` Carrier In wasmparser

**Files:**

- Modify:
  `/home/shisoft/Code/Research/wasm-tools-transaction/crates/wasmparser/src/readers/core/types.rs`

- [x] **Step 1.1: Add ordinary reference permission regression tests**

In the existing `#[cfg(test)] mod tests`, add tests named:

- `ordinary_ref_types_have_no_transaction_permission`
- `transaction_permission_subtyping_is_write_read_none`
- `transaction_permission_round_trips_without_changing_ref_identity`

The tests should prove:

- built-in ordinary refs such as `RefType::FUNCREF`, `RefType::EXTERNREF`,
  `RefType::ANYREF`, `RefType::STRUCTREF`, `RefType::ARRAYREF`, and concrete
  refs all report `TransactionRefPermission::None` through constructors and
  real byte decoding
- `Write` is a subtype of `Read`, `Read` is a subtype of `None`, and weaker
  permissions are not subtypes of stronger permissions
- `with_transaction_permission` changes only the permission carrier and
  preserves nullability and heap type

Run:

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo test -p wasmparser ordinary_ref_types_have_no_transaction_permission --lib
cargo test -p wasmparser transaction_permission_subtyping_is_write_read_none --lib
cargo test -p wasmparser transaction_permission_round_trips_without_changing_ref_identity --lib
```

Expected: all commands exit 0.

---

## Task 2: Document The Boundary In Wasmtime

**Files:**

- Modify: `docs/shisoft/transactional-wasm-runtime-core-design.md`
- Modify:
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md`
- Modify:
  `docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md`
- Modify: `docs/shisoft/transactional-wasm-implementation-log.md`

- [x] **Step 2.1: Add the core-design note**

Add a short section under parser/validation/lowering implications explaining:

- runtime permissions live in `TransactionState` and lock/version state keyed by
  `GranuleId`
- persistent object runtime access maps live refs to
  `GranuleId::Object(ObjectId)` before acquiring permissions
- `RefType.transaction_permission` is a temporary parsed/validated type-state
  carrier used to thread permission metadata through the existing wasmparser and
  Cranelift interfaces
- ordinary volatile references are still ordinary `RefType`s with
  `TransactionRefPermission::None`
- a future `TRefType` split should remove transaction permission metadata from
  ordinary `RefType`

- [x] **Step 2.2: Add the `TRefType` migration note**

In the roadmap and implementation plan, mark Wave 7 as the boundary wave and
state the prerequisites for a future `TRefType` split:

- final `ObjectId`-carrying live ABI
- restart-stable durable function/external reference identity
- complete transaction WAST and fuzz coverage across parser, validator,
  lowering, and runtime

Do not implement `TRefType` in this wave.

- [x] **Step 2.3: Update the implementation log**

Add a dated Wave 7 entry that records:

- wasmparser tests now pin ordinary refs to `TransactionRefPermission::None`
- the current permission carrier is temporary and not runtime object state
- full `TRefType` remains deferred

---

## Task 3: Verify Both Repositories

**Files:**

- Verify all files changed by Tasks 1 and 2.

- [x] **Step 3.1: Run wasmparser checks**

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
cargo fmt --check
cargo test -p wasmparser ordinary_ref_types_have_no_transaction_permission --lib
cargo test -p wasmparser transaction_permission_subtyping_is_write_read_none --lib
cargo test -p wasmparser transaction_permission_round_trips_without_changing_ref_identity --lib
cargo check -p wasmparser --lib
```

Expected: all commands exit 0.

- [x] **Step 3.2: Run Wasmtime checks**

```sh
cd /home/shisoft/Code/Research/wasmtime
cargo fmt --check
git diff --check
rg "transaction_permission|TRefType" crates docs
cargo check -p wasmtime-fuzzing --lib
```

Expected: all commands exit 0. The `rg` output should show the temporary
carrier and `TRefType` deferral only; it should not show runtime object
permissions depending on `RefType`.

---

## Task 4: Review And Commit Wave 7

**Files:**

- Review all files changed by Tasks 1 through 3.

- [x] **Step 4.1: Run spec and quality reviews**

Use subagent-driven development review gates:

- spec review: verify ordinary refs stay permission-free, docs do not overclaim,
  and `TRefType` remains deferred
- quality review: verify tests are meaningful and docs stay consistent with the
  object-model roadmap

- [x] **Step 4.2: Commit wasm-tools fork**

Commit without any `Co-authored-by` annotation:

```sh
cd /home/shisoft/Code/Research/wasm-tools-transaction
git add crates/wasmparser/src/readers/core/types.rs
git commit -m "Pin transaction reference permission carrier"
```

- [x] **Step 4.3: Commit Wasmtime docs**

Commit without any `Co-authored-by` annotation:

```sh
cd /home/shisoft/Code/Research/wasmtime
git add docs/shisoft/2026-06-16-wave7-type-system-cleanup-boundary-plan.md \
  docs/shisoft/transactional-wasm-runtime-core-design.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-roadmap.md \
  docs/shisoft/2026-06-16-transactional-object-model-stabilization-implementation-plan.md \
  docs/shisoft/transactional-wasm-implementation-log.md
git commit -m "Document transaction reference type boundary"
```
