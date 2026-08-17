# Spec-Native Transaction Conformance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make transactional Wasmtime and its patched wasm-tools frontend conform exactly to `wasm-persistence` simple-transactions at `9a1151b41b21187f11feb4fbc561175f9ea751e0`, including binary interoperability with the reference interpreter and a fully enabled, green proposal WAST suite.

**Architecture:** Replace the custom-section/ordinary-index compatibility model with explicit transactional namespaces in wasm-tools and Wasmtime. Preserve the entity namespace independently of a value's `ref`/`tref` type, carry namespace-qualified operands and indirect-call flags from WAT/raw bytes through validation and Cranelift lowering, and give transactional tables, memories, globals, elements, and data their own module collections and instance mappings. Treat structured transactional control flow and the WAST reference boundary as first-class runtime behavior, not test-harness rewrites.

**Tech Stack:** Rust; local `../wasm-tools-transaction` patches for `wast`, `wasmparser`, `wasm-encoder`, and `wat`; Wasmtime environ, Cranelift, runtime transaction support, libtest-mimic WAST integration tests; `wasm-persistence` proposal fixtures and reference interpreter as the compatibility oracle.

## Global Constraints

- The target format and behavior are the current proposal only. Reject the former `0xf0` table marker and every old/custom compatibility encoding; do not translate it.
- The implementation must accept and emit the proposal's actual WebAssembly bytes. Passing a WAT-only path is insufficient.
- `table`/`ttable`, `memory`/`tmemory`, `global`/`tglobal`, `elem`/`telem`, and `data`/`tdata` are distinct namespaces. Entity namespace is independent of `ref` versus `tref` element/value type.
- `shisoft.transaction.objects` is not a semantic input or output of the spec-native frontend/runtime. Remove its production use and update internal rewrite tools/tests that assume it.
- `flags=n` for indirect calls uses the proposal's packed `u64` immediate, defaults per instruction, and is validated before lowering.
- The relevant corpus is every discovered `*.wast` fixture under `../wasm-persistence/test/core/simple-transactions` and `../wasm-persistence/test/core/tsimd`, plus a Rust port of `tconflict-tmemory.py`. No per-fixture allowlist or ignored test is permitted.
- Local-variable state after abort remains intentionally unspecified; tests must not assert either rollback or retention.
- Follow the Bytecode Alliance AI Tool Use Policy. Do not open, review, or comment on Wasmtime pull requests or issues. Do not add an agent, model, or tool to commit authorship annotations.
- Keep the user-owned untracked `.dropboxignore` and `reports/` untouched. Make commits only after a task's specified checks pass, with normal human Git identity and no push.

---

## File Structure

- `../wasm-tools-transaction/crates/wast/src/core/{table,memory,import,export,expr}.rs` and `core/resolve/{names,types}.rs`: typed WAT entity namespaces, names, `tblock`/`ttry`, and indirect-call flags.
- `../wasm-tools-transaction/crates/wasmparser/src/{lib.rs,readers/core/{tables,memories,elements,data,imports,exports,types}.rs,validator/**}`: current binary decoding and validation.
- `../wasm-tools-transaction/crates/wasm-encoder/src/core/{tables,elements,data,imports,exports,code,instructions}.rs`: current binary emission and round trips.
- `../wasm-tools-transaction/crates/wat/src/lib.rs` and `crates/wasmprinter/src/operator.rs`: public WAT conversion/printer coverage after typed AST changes.
- `crates/environ/src/{types,module,transaction}.rs` and `crates/environ/src/compile/module_environ.rs`: separate module indexes/collections, no custom-section semantics, validation, and translation metadata.
- `crates/cranelift/src/{translate/code_translator.rs,func_environ.rs}`: namespace-qualified operators, indirect calls, const expressions, and transactional control frames.
- `crates/wasmtime/src/runtime/{vm/instance.rs,func.rs,store.rs,transaction/**}`: instance layout, import/export mapping, structured abort behavior, and a real transaction-reference ABI at the WAST boundary.
- `crates/test-util/src/wast.rs`, `tests/wast.rs`, and `crates/wast/src/wast.rs`: direct fixture discovery, configuration, and spec-runner behavior.
- `crates/transaction-tools/**`: remove obsolete custom-section production/expectations where those tools synthesize transaction modules.

---

### Task 1: Define the Native Frontend Model and Exact Binary Contracts

**Files:**

- Modify: `../wasm-tools-transaction/crates/wasmparser/src/readers/core/{types,tables,memories,elements,data,imports,exports}.rs`
- Modify: `../wasm-tools-transaction/crates/wasmparser/src/binary_reader.rs`
- Modify: `../wasm-tools-transaction/crates/wasm-encoder/src/core/{tables,elements,data,imports,exports}.rs`
- Modify: the unit-test modules next to each edited reader/encoder.

**Interfaces:**

- Introduce a shared public discriminator such as `EntityNamespace::{Ordinary, Transactional}` and expose it on table, memory, global, element, and data types; do not infer it from `RefType`.
- Extend `TypeRef`, `ExternalKind`, `wasm_encoder::EntityType`, and `wasm_encoder::ExportKind` with native transactional table/memory/global forms.
- `TableType` and `MemoryType` retain their type properties but encode/decode bit `0x40` as the namespace bit. `GlobalType` does the same in its mutability byte. `Element` and `Data` retain a separate segment namespace bit (`0x20` and `0x40`, respectively); `ElementItems` retains the independent `tref` bit.

- [ ] **Step 1: Add byte-level RED tests for every changed discriminant.**

  In wasmparser tests, decode raw section payloads that cover:

  - ordinary and transactional tables carrying both ordinary and transactional reference types;
  - `0x40`/`0x41` table and memory limit encodings and `0x40`/`0x41` global mutability encodings;
  - import/export kinds `0x41`, `0x42`, `0x43`;
  - all meaningful combinations of element low mode bits with `0x20` (`telem`) and `0x40` (`tref`);
  - data flags `0x40`, `0x41`, and `0x42`; and
  - the former `[0xf0, 0x7d, ...]` table payload, which must now be rejected.

  Mirror these in wasm-encoder as exact-byte encode assertions and parser/encoder round trips. The red tests must fail because the current reader treats `0xf0` as the table marker and strips transaction bits from data/elements.

- [ ] **Step 2: Add the explicit namespace data model.**

  Put the discriminator where it represents the *entity or segment*, not on `RefType`. Replace the current `TableType::transaction`/`MemoryType::transaction`-only convention with consistently named namespace-bearing fields/types. Add `Element::namespace` and `Data::namespace`; leave `ElementItems::Expressions(RefType, ...)` responsible only for its reference type.

  Add `TypeRef::{TTable, TMemory, TGlobal}` and matching external-kind variants. Make the new imports/exports encode with the specified bytes rather than normal entity kinds plus metadata.

- [ ] **Step 3: Implement strict current-format decode and encode.**

  - Parse table flags after the element type, allowing the normal limits bits plus `0x40`, and reject every other high bit.
  - Parse memory/global namespace bits without losing the remaining standard flags.
  - Split element flags into `mode = flags & 0x07`, `namespace = flags & 0x20`, and `reference_namespace = flags & 0x40`; reject bits outside the proposal plus standard low bits.
  - Split data flags into `mode = flags & 0x03` and `namespace = flags & 0x40`, rejecting invalid combinations.
  - Encode the same fields with one canonical path; never write the legacy table prefix or a compatibility custom section.

- [ ] **Step 4: Propagate the new cases to all exhaustive matches.**

  Update parser visitors, debug formatting, `wat` conversion, `wasmprinter`, fuzz/round-trip helpers, and every `TypeRef`/`ExternalKind`/encoder `EntityType` match so no fallback silently converts a transactional entity into an ordinary one.

- [ ] **Step 5: Verify the focused frontend boundary.**

  ```bash
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml \
    -p wasmparser -p wasm-encoder -p wast -p wat --lib -- --format terse
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml \
    -p wasmparser transactional -p wasm-encoder transactional -- --format terse
  ```

  Require exact byte assertions and legacy-byte rejection to pass before proceeding.

- [ ] **Step 6: Commit the frontend model.**

  ```bash
  git -C ../wasm-tools-transaction add crates/{wasmparser,wasm-encoder,wast,wat,wasmprinter}
  git -C ../wasm-tools-transaction commit -m "Model native transaction entity namespaces"
  ```

---

### Task 2: Parse, Resolve, Validate, and Encode Namespace-Qualified Indirect Calls

**Files:**

- Modify: `../wasm-tools-transaction/crates/wast/src/core/{expr,table,memory,import,export}.rs`
- Modify: `../wasm-tools-transaction/crates/wast/src/core/resolve/{names,types}.rs`
- Modify: `../wasm-tools-transaction/crates/wasmparser/src/{lib.rs,readers/core/operators.rs,validator/operators.rs,validator/operators/transaction.rs}`
- Modify: `../wasm-tools-transaction/crates/wasm-encoder/src/core/{code,instructions}.rs`
- Modify: focused parser/validator/encoder tests in those modules.

**Interfaces:**

- `CallIndirect` (including `TCallIndirect`, `ReturnCallIndirect`, and `ReturnTCallIndirect`) carries `Option<u31>` flags and a table operand whose namespace remains known after resolution.
- Use one helper equivalent to `pack_indirect_table_immediate(default_flags, flags, table_index)` and its inverse. It owns the packed proposal `u64`: low 32 bits table index; a zero high word means use the instruction default; otherwise it is an odd shifted flag encoding.
- Resolver namespace maps distinguish normal and transactional tables, memories, elements, and data. Transaction operators resolve only their proposal-defined namespace, except a flagged `tcall_indirect` lookup from non-transactional code may select a normal table before it begins the transaction.

- [ ] **Step 1: Add WAT and raw-byte RED coverage.**

  Add tests that parse and encode all four indirect forms with omitted flags and explicit `flags=0`/`flags=1`, including a `tcall_indirect flags=0` from ordinary code. Assert the produced immediate byte sequence is exactly the proposal packing and that malformed high words, even nonzero flag encodings, overflow, and invalid `flags=n` syntax fail validation.

  Add WAT resolver tests that define identically named/index-zero `table` and `ttable`, `elem` and `telem`, plus `memory` and `tmemory`, then prove each ordinary/transaction instruction resolves only its correct space. Include both `ref` and `tref` elements in both segment spaces.

- [ ] **Step 2: Make text syntax preserve entity namespace.**

  Replace the booleans that conflate `ttable` with `tref` in `core/table.rs` and the analogous memory/data/import/export paths. Add explicit `telem`/`tdata` forms and make inline module fields retain their declaration namespace. Parse optional `flags=n` before the type/table use and populate the richer `CallIndirect` AST rather than applying an early default.

- [ ] **Step 3: Split resolver name/index maps.**

  Add separate resolver namespaces for tables, memories, elements, data, and globals. Update every `TTable*`, `TMemory*`, `TData*`, `TGlobal*`, transactional array initializer, and indirect-call name resolution site currently using `Ns::Table`, ordinary element/data maps, or an ordinary default index. A wrong-space name/index must produce a source-level validation error before encoding.

- [ ] **Step 4: Implement common indirect immediate codec.**

  Add the shared pack/unpack helper in wasmparser/encoder's common transaction instruction path and use it for normal, transactional, and tail forms. The decoder applies default `0` to normal calls and default `1` to transactional calls only when the packed high word is zero. Keep the resolved namespace/flags in the operator instead of recovering it from reference type or transaction state.

- [ ] **Step 5: Extend validation and encoder instruction forms.**

  Add explicit `Operator::{CallIndirect,TCallIndirect,ReturnCallIndirect,ReturnTCallIndirect}` operand fields/types that include the selected namespace/flags. Validate:

  - ordinary calls select only ordinary tables;
  - an in-transaction `tcall_indirect` selects a transactional table;
  - a transactional call that starts outside a transaction may select an ordinary table only with the permitted flag; and
  - selected table element type and callable function type are still checked normally.

  Encode only the new packed immediate. Remove the previous `TCallIndirect` alias-to-ordinary-call behavior.

- [ ] **Step 6: Verify targeted frontend suites.**

  ```bash
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml \
    -p wast -p wasmparser -p wasm-encoder indirect -- --format terse
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml \
    -p wast transaction -- --format terse
  ```

- [ ] **Step 7: Commit.**

  ```bash
  git -C ../wasm-tools-transaction add crates/{wast,wasmparser,wasm-encoder,wat,wasmprinter}
  git -C ../wasm-tools-transaction commit -m "Encode transactional indirect-call namespaces"
  ```

---

### Task 3: Replace Custom Metadata with Native Wasmtime Transaction Spaces

**Files:**

- Modify: `crates/environ/src/{types,module,transaction}.rs`
- Modify: `crates/environ/src/compile/module_environ.rs`
- Modify: `crates/wasmtime/src/runtime/{vm/instance.rs,memory.rs,store.rs}` and the transaction table/memory/global runtime modules reached by their callers.
- Modify: `crates/transaction-tools/src/{kotlin,rewrite.rs,rust/rewrite.rs}` and their tests if they emit or consume `shisoft.transaction.objects`.
- Modify: environ/unit tests and add a dedicated translation regression module if needed.

**Interfaces:**

- Add `TTableIndex`, `TMemoryIndex`, `TGlobalIndex`, `TElemIndex`, and `TDataIndex` alongside ordinary index newtypes in `crates/environ/src/types.rs`.
- `Module` owns separate primary maps/segment collections and import counts for transactional entities. Explicit conversion/accessor methods map a namespace-qualified validated operand to its corresponding instance slot; no `is_t*` predicate over an ordinary index remains.
- Remove `TransactionObjectMetadata`, `decode_transaction_object_metadata`, `merge_transaction_object_metadata`, and `TRANSACTION_OBJECTS_CUSTOM_SECTION` from the semantic module load path. Reject or ignore no old input through a compatibility path; the old binary form is invalid under this target.

- [ ] **Step 1: Add translation RED tests for overlapping namespaces.**

  Construct raw modules (not just WAT) containing ordinary and transactional table/memory/global/element/data index zero simultaneously. Assert translation preserves independent counts, import slots, exports, active initializers, and passive-segment operands. Add negative modules proving an ordinary instruction cannot refer to a transactional index and vice versa.

  Add a module containing the old custom section and old table marker; expect decode/translation rejection. These tests must fail with today's ordinary-index metadata model.

- [ ] **Step 2: Define distinct indexes, module fields, and initializers.**

  Add the five transaction index newtypes and separate `TryPrimaryMap`/segment structures in `Module`. Split table/memory/global initializer bookkeeping where a namespace-specific runtime operation is required. Keep the ordinary maps unchanged so normal Wasm behavior stays isolated. Model transactional imports and exports with typed `EntityIndex`/initializer variants rather than an ordinary entity plus a marker.

- [ ] **Step 3: Translate every section natively.**

  In `module_environ.rs`, use the new wasmparser `TypeRef`/`ExternalKind` cases to allocate the appropriate space for imports. Populate normal versus transactional collections independently for table, memory, global, element, and data sections. Route active element/data segment table/memory operands through the namespace encoded by the segment. Make `tglobal.get` constant expressions resolve preceding immutable transactional globals in their own space and reject mutable/forward/wrong-space references.

- [ ] **Step 4: Remove metadata semantics and update internal tools.**

  Delete custom-section decoding, serialization, merging, and producer code. Rewrite transaction-tool tests to inspect native section/entity bytes and translated indices instead of custom metadata. If a transaction tool cannot yet produce a native construct, make that lack explicit and add the native emission rather than retaining a side channel.

- [ ] **Step 5: Build native instance mappings.**

  Update `InstanceHandle`, VMContext allocation, imports, exports, table/memory/global accessors, and startup initialization to select their own namespace collection. Ensure native `tdata`/`telem` active and passive initialization never aliases normal runtime data/passive element slots merely because numeric indexes coincide.

- [ ] **Step 6: Verify translation and runtime layout.**

  ```bash
  cargo test -p wasmtime-environ transaction --lib -- --format terse
  cargo test -p wasmtime --features transaction transaction --lib -- --format terse
  cargo test -p wasmtime --features transaction --test transaction_persistence -- --format terse
  ```

  Also inspect with `wasmparser::Parser` and `wasm_encoder` in the test to prove the generated module has no `shisoft.transaction.objects` custom section.

- [ ] **Step 7: Commit.**

  ```bash
  git add crates/environ crates/wasmtime/src/runtime crates/transaction-tools
  git commit -m "Use native transaction index spaces"
  ```

---

### Task 4: Carry Qualified Operands Through Wasmtime Validation and Cranelift

**Files:**

- Modify: `crates/environ/src/compile/module_environ.rs` and transaction validation helpers.
- Modify: `crates/cranelift/src/translate/code_translator.rs`
- Modify: `crates/cranelift/src/func_environ.rs` and transaction-specific lowering helpers.
- Modify: focused environ/Cranelift transaction tests.

**Interfaces:**

- Replace `TransactionObjectMetadata::is_t*` lookups with operand/index types that cannot name the wrong space.
- Add Cranelift lowering entry points for `TCallIndirect` and `ReturnTCallIndirect`; normal and transactional calls receive an explicit table namespace and flags.
- Table/memory/global/element/data operations use namespace-specific lookup/overlay helpers. The only ordinary-table path in a transactional indirect call is the validated, outside-transaction flag-selected lookup before transaction start.

- [ ] **Step 1: Add lowering RED cases.**

  Add compile-and-execute tests for:

  - numeric index zero in an ordinary table and a transactional table with different functions, proving normal versus transactional indirect calls choose the requested one;
  - `tcall_indirect flags=0` from non-transactional code, proving table lookup happens before transaction creation and the called `tfunc` then runs transactionally;
  - rejection of `flags=0` from inside a transaction and wrong-space table/memory/global/segment operands; and
  - native `tglobal.get` initializer ordering and value at instantiation.

  The initial tests should expose the absent transactional indirect operator and today’s ordinary-index overlay inference.

- [ ] **Step 2: Update environ validation interfaces.**

  Make validated operators carry namespace-qualified index newtypes into function translation. Replace generic integer/index conversions that would allow `TableIndex(0)` to stand for `TTableIndex(0)`. Validate the special transaction-start `tcall_indirect flags=0` case at the point that knows current transaction state; issue a validation error for all other cross-space combinations.

- [ ] **Step 3: Add distinct instruction translation paths.**

  In `code_translator.rs`, match the new wasmparser `TCallIndirect` and tail form and call new `FuncEnvironment` methods rather than reusing `CallIndirect`. In `func_environ.rs`, select normal table storage for the one permitted pre-start lookup, then establish/reuse the transaction before invoking the tfunction. All other transactional table operations use `TTableIndex`; all normal ones use `TableIndex`.

- [ ] **Step 4: Convert bulk and initialization lowering.**

  Route `TMemoryInit`, `TDataDrop`, `TTableInit`, `TElemDrop`, table copy/fill/grow/size/get/set, transactional array data/elem operations, and startup active segments through the correct native maps. Preserve all existing bounds/type checks and transactional read/write-set operations; only replace the metadata-based entity selection.

- [ ] **Step 5: Verify compiler execution.**

  ```bash
  cargo test -p wasmtime-environ transaction --lib -- --format terse
  cargo test -p wasmtime-cranelift transaction --lib -- --format terse
  cargo test -p wasmtime --features transaction transaction --lib -- --format terse
  ```

- [ ] **Step 6: Commit.**

  ```bash
  git add crates/environ crates/cranelift crates/wasmtime/src/runtime
  git commit -m "Lower namespace-qualified transaction operands"
  ```

---

### Task 5: Implement Structured Transactional Blocks and Const-Expression Semantics

**Files:**

- Modify: `../wasm-tools-transaction/crates/wast/src/core/expr.rs` and resolver/type tests.
- Modify: `../wasm-tools-transaction/crates/wasmparser/src/validator/operators/transaction.rs` and const-expression validation paths.
- Modify: `crates/cranelift/src/{translate/code_translator.rs,func_environ.rs}`.
- Modify: `crates/environ/src/compile/module_environ.rs` and Wasmtime transaction tests.

**Interfaces:**

- `tblock` is represented as structured `TTryStart`/`TTryElse`/`TTryEnd` control flow with labels and block type, not a success body plus a parsed/discarded abort body.
- Transaction frame helpers own start, abort transfer, success completion, and terminal cleanup exactly once. Locals remain unspecified after abort.
- Constant-expression validation accepts `tglobal.get` only for an earlier immutable transactional global in the correct namespace.

- [ ] **Step 1: Add focused `tblock`/`ttry` RED tests.**

  Add WAT parser tests that retain labels, result types, branches, `return`, and an `(else ...)` handler. Add execution tests where the success arm yields one value, abort yields another, nested transactional blocks route to the correct handler, and branches/returns exit the intended frame. Do not assert local values after abort.

  Add constant-expression tests for legal preceding immutable `tglobal.get` and illegal forward, mutable, normal-global, and wrong-namespace cases. The current `handle_tblock_lparen` must fail the handler-execution tests because it discards the handler.

- [ ] **Step 2: Parse the complete control frame.**

  Replace the `SHISOFT_TRANSACTION_SCAFFOLD` path in `handle_tblock_lparen` with the same expression/label construction discipline used by standard blocks and `ttry`. Preserve the parsed `BlockType`, arm spans, and label stack. Emit `TTryStart`, optional `TTryElse`, and `TTryEnd` in the real instruction stream; do not flatten or discard either arm.

- [ ] **Step 3: Validate control and const expressions.**

  Extend transaction validator control frames so branch targets, arity, reachability, and handler parameter/result values follow the proposal. Update const-expression operator admission and module context construction for `tglobal.get` without accidentally admitting a normal `global.get` across spaces.

- [ ] **Step 4: Lower success, abort, and cleanup paths.**

  Adapt existing `translate_transaction_structured_try_start`, `_else`, and `_end` helpers to use the fully preserved frame. Make abort supply the handler’s failure value, execute exactly one handler, and clean up once if this frame started the transaction. Retain non-local cleanup on every branch/return path; avoid a local snapshot mechanism.

- [ ] **Step 5: Run targeted control tests and fixtures.**

  ```bash
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml -p wast tblock -- --format terse
  cargo test -p wasmtime --features transaction ttry --lib -- --format terse
  WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast \
    'transaction-proposal/simple-transactions/tfunc_block.wast' -- --exact --format terse
  ```

- [ ] **Step 6: Commit each repository’s completed portion.**

  ```bash
  git -C ../wasm-tools-transaction add crates/wast crates/wasmparser
  git -C ../wasm-tools-transaction commit -m "Preserve transactional abort handlers"
  git add crates/environ crates/cranelift crates/wasmtime/src/runtime
  git commit -m "Execute structured transactional abort handlers"
  ```

---

### Task 6: Replace the Test-Only Transaction Reference ABI Shim

**Files:**

- Modify: `crates/wasmtime/src/runtime/{func.rs,store.rs,values.rs}` and the transaction durable-reference/object-table modules they call.
- Modify: `crates/wast/src/wast.rs` only if its special test-store setup becomes unnecessary.
- Modify: `crates/wasmtime/src/runtime/transaction/tests.rs` and add a focused WAST/embedding regression test.

**Interfaces:**

- A typed transactional reference crosses Wasm function call/return boundaries using an explicit transaction reference representation; it is never passed to the generic GC `AnyRef`/`ExternRef` conversion merely because its raw bits look like one.
- Remove the `SHISOFT-TWASM-MOCK` `texterntref` exception and make `transaction_wast_result_*` either a narrow fully typed adapter or unnecessary. Test behavior must not depend on `WASMTIME_TEST_TRANSACTION_WAST` enabling a semantic fallback.

- [ ] **Step 1: Freeze the three failures as small RED regressions.**

  Add exact tests that execute the minimal object-return programs underlying `tarray.wast`, `tstruct.wast`, and `tconflict-tmemory_1.wast`. Each test must assert a returned transactional object/reference can be consumed by its next transaction operation without allocating/accessing the ordinary GC heap. Run them with a backtrace once to confirm the pre-fix path reaches `Val::from_raw` and panics in `AnyRef`/`GcStore`.

- [ ] **Step 2: Trace and type the raw-reference boundary.**

  Follow the `Func::call_impl_do_call` and typed-call result paths to identify the result `ValType`, raw tag, and object-table handle for each RED program. Add an internal enum/typed helper for transaction object/i31/extern identities so raw handles are classified from the transaction ABI, not guessed from permitted `HeapType` values.

- [ ] **Step 3: Implement the narrow ABI repair.**

  Make argument marshalling and result unmarshalling dispatch through the transaction-reference representation before generic `Val::to_raw`/`Val::from_raw`. Preserve nullability, concrete array/struct type checks, i31, function, and external identities. Reject incompatible values with the normal Wasm type error; never reinterpret them as GC-managed raw references.

- [ ] **Step 4: Remove the fallback-only semantic path.**

  Delete the raw `Val::I32`-as-reference WAST escape hatch and the `HeapType::Extern => true` mock behavior once tests use the typed boundary. Keep test configuration only for non-semantic harness needs; remove `enable_live_wast_reference_fallbacks_for_test` if nothing legitimately uses it.

- [ ] **Step 5: Verify all focused and GC-adjacent coverage.**

  ```bash
  cargo test -p wasmtime --features transaction transaction_wast --lib -- --format terse
  WASMTIME_TEST_TRANSACTION_WAST=1 cargo test --test wast \
    'transaction-proposal/simple-transactions/(tarray|tstruct|tconflict-tmemory_1)\.wast' \
    -- --test-threads=1 --format terse
  ```

  Require no GC-heap-before-allocation, object-out-of-bounds, or mock fallback path.

- [ ] **Step 6: Commit.**

  ```bash
  git add crates/wasmtime/src/runtime crates/wast
  git commit -m "Type transactional references at the Wasm boundary"
  ```

---

### Task 7: Make the Spec Corpus a Direct, Complete Gate

**Files:**

- Modify: `crates/test-util/src/wast.rs`
- Modify: `tests/wast.rs`
- Modify: `crates/wast/src/wast.rs` only for runner-level, non-semantic setup.
- Add: a Rust integration test under the appropriate existing `crates/wasmtime/tests/` or `tests/` location that ports `../wasm-persistence/test/core/simple-transactions/tconflict-tmemory.py`.

**Interfaces:**

- Every discovered WAST under the two proposal directories has `transaction_proposal_enabled() == true` and `transaction_real_text_parser() == true` without a filename list.
- Per-directory configuration supplies necessary proposal features (for example SIMD for `tsimd`), not permission to skip individual files.
- The Python generator’s behavior is represented in a deterministic Rust test that instantiates the generated type/value combinations and checks its expected conflict results.

- [ ] **Step 1: Add discovery RED tests.**

  Enumerate the two fixture directories from the test utility and assert every `*.wast` is enabled and real-text parsed. Assert `tfunc_block.wast` is found. Do not assert a count; the test must fail if future upstream files are unconfigured. Verify the `tests/wast.rs` trial creation has no ignored proposal case.

- [ ] **Step 2: Replace the static allowlists.**

  Delete `SIMPLE_TRANSACTION_REAL_TEXT_*`, `SIMPLE_TRANSACTION_REAL_BINARY`, and the static `tsimd` name match. Make `transaction_proposal_enabled` and `transaction_proposal_uses_real_text_parser` depend solely on `TransactionProposalSuite` and discovery root. Retain diagnostic normalization only where it translates equivalent assertion diagnostics, never module bytes or syntax.

- [ ] **Step 3: Port `tconflict-tmemory.py`.**

  Translate its deterministic generated template/case matrix into Rust test data. Invoke the native Wasmtime WAT/raw-binary path, run the same conflict schedules, and assert the returned codes/values. Avoid shelling out to Python or copying generated fixture output into the repository; the Rust test should express the cases and expected behavior directly.

- [ ] **Step 4: Drive fixture failures to root causes one at a time.**

  Start with all direct tests enabled. For each failure, reduce to the named fixture plus a minimal regression test, identify whether the first mismatch is parser, validation, encoding, translation, lowering, or runtime, then make the narrow change in the owning task area. Do not add ignore entries, fixture-specific transformations, or expected-failure exceptions.

- [ ] **Step 5: Verify the full proposal gate.**

  ```bash
  WASMTIME_TEST_TRANSACTION_WAST=1 \
    cargo test --test wast transaction-proposal -- --test-threads=1
  ```

  Require every discovered transaction proposal test to run, with **zero failures and zero ignored**. Record the dynamic discovered/running count in the test log, but do not hard-code it.

- [ ] **Step 6: Commit.**

  ```bash
  git add crates/test-util crates/wast tests crates/wasmtime/tests
  git commit -m "Run the complete transaction proposal corpus"
  ```

---

### Task 8: Cross-Implementation Bytes, Regression Matrix, and Final Audit

**Files:**

- Modify only evidence-backed files discovered by this task.
- Update: `reports/findings/2026-08-17-wasm-persistence-spec-drift.md` only if the user wants the local untracked audit report kept current; do not stage it otherwise.

**Interfaces:**

- Consumes the spec-native frontend/runtime from Tasks 1–7.
- Produces reproducible evidence that Wasmtime accepts/emits current proposal bytes, executes the full relevant corpus, and leaves no prohibited compatibility/ignore mechanism.

- [ ] **Step 1: Run the patched toolchain suites.**

  ```bash
  cargo test --manifest-path ../wasm-tools-transaction/Cargo.toml \
    -p wasmparser -p wasm-encoder -p wast -p wat --all-targets -- --format terse
  ```

  Fix only reproducible failures caused by the new native model. Retain a test for each correction.

- [ ] **Step 2: Verify reference-interpreter byte compatibility.**

  For a curated set covering imports/exports, `ttable` with `ref` and `tref`, `telem`/`tdata`, separate zero indexes, every indirect-call flag/default, `tblock`, and `tglobal.get` initialization:

  1. encode with `wasm-encoder`/`wat`;
  2. validate/decode with `wasmparser` and Wasmtime;
  3. validate/run with the checked-out `../wasm-persistence/interpreter`; and
  4. compare the exact bytes and observable results.

  Add a permanent Rust test or checked-in binary-byte fixture for each byte contract. Do not maintain a translator for historical encodings.

- [ ] **Step 3: Run Wasmtime’s focused and ordinary gates.**

  ```bash
  cargo test -p wasmtime-environ transaction --lib -- --format terse
  cargo test -p wasmtime-cranelift transaction --lib -- --format terse
  cargo test -p wasmtime --features transaction transaction --lib -- --format terse
  cargo test -p wasmtime --features transaction --test transaction_persistence -- --format terse
  WASMTIME_TEST_TRANSACTION_WAST=1 \
    cargo test --test wast transaction-proposal -- --test-threads=1
  cargo test --test wast -- --test-threads=1
  ```

  The transaction proposal command requires zero failures and zero ignored. If the ordinary full WAST command reveals unrelated pre-existing failures, report them separately with the exact command/output; do not weaken its existing policy.

- [ ] **Step 4: Format and inspect both repositories.**

  ```bash
  cargo fmt --manifest-path ../wasm-tools-transaction/Cargo.toml --all -- --check
  cargo fmt --all -- --check
  git -C ../wasm-tools-transaction diff --check
  git diff --check
  rg -n 'SHISOFT_TRANSACTION_SCAFFOLD|SHISOFT-TWASM-MOCK|transaction\.objects|SIMPLE_TRANSACTION_REAL_TEXT|with_ignored_flag' \
    crates tests ../wasm-tools-transaction/crates
  ```

  The search must show no production compatibility scaffold, custom semantic metadata, filename allowlist, or proposal-test ignore path. Evaluate any legitimate documentation/test occurrence before deletion.

- [ ] **Step 5: Audit commits and working state.**

  ```bash
  git status --short
  git -C ../wasm-tools-transaction status --short
  git log --format='%H %s%n%an <%ae>%n%cn <%ce>%n%b' a3175d119d265e77fddfe1eae88056f9bae85a4f..HEAD
  git -C ../wasm-tools-transaction log --format='%H %s%n%an <%ae>%n%cn <%ce>%n%b' HEAD~8..HEAD
  ```

  Require no unintended changes, no AI/tool co-author annotation, no push, and no PR/issue activity. Preserve the known user-owned untracked files noted in the global constraints.

- [ ] **Step 6: Report the implementation evidence.**

  Report the two repository commit ranges, byte-compatibility cases, full proposal suite result/count/ignored count, conflict-port result, focused/ordinary WAST results, and any intentionally unrun environment-specific checks.
