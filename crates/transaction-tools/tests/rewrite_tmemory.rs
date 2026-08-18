use wasmparser::{EntityNamespace, Parser, Payload, TypeRef};
use wasmtime_transaction_tools::{RewriteReport, rewrite_module};

const OBSOLETE_TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";

#[derive(Debug, Default, Eq, PartialEq)]
struct TransactionObjects {
    memories: Vec<u32>,
    globals: Vec<u32>,
    functions: Vec<u32>,
    tables: Vec<u32>,
}

fn rewrite_and_print(wat: &str) -> (Vec<u8>, RewriteReport, String) {
    let input = wat::parse_str(wat).expect("fixture parses");
    let (output, report) = rewrite_module(&input).expect("rewrite succeeds");
    let printed = wasmprinter::print_bytes(&output).expect("printed wat");
    (output, report, printed)
}

fn transaction_objects(bytes: &[u8]) -> (usize, TransactionObjects) {
    let mut obsolete_sections = 0;
    let mut native = TransactionObjects::default();
    let mut function_types = Vec::new();
    let mut next_function = 0u32;

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("parse payload");
        match payload {
            Payload::TypeSection(section) => {
                function_types.extend(
                    section
                        .into_iter_err_on_gc_types()
                        .map(|ty| ty.expect("function type").transaction()),
                );
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    match import.expect("import").ty {
                        TypeRef::Func(ty) | TypeRef::FuncExact(ty) => {
                            if function_types[ty as usize] {
                                native.functions.push(next_function);
                            }
                            next_function += 1;
                        }
                        TypeRef::TMemory(_) => native.memories.push(native.memories.len() as u32),
                        TypeRef::TGlobal(_) => native.globals.push(native.globals.len() as u32),
                        TypeRef::TTable(_) => native.tables.push(native.tables.len() as u32),
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(section) => {
                for ty in section {
                    let ty = ty.expect("function type index");
                    if function_types[ty as usize] {
                        native.functions.push(next_function);
                    }
                    next_function += 1;
                }
            }
            Payload::MemorySection(section) => {
                for ty in section {
                    if ty.expect("memory type").namespace == EntityNamespace::Transactional {
                        native.memories.push(native.memories.len() as u32);
                    }
                }
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    if global.expect("global").ty.namespace == EntityNamespace::Transactional {
                        native.globals.push(native.globals.len() as u32);
                    }
                }
            }
            Payload::TableSection(section) => {
                for table in section {
                    if table.expect("table").ty.namespace == EntityNamespace::Transactional {
                        native.tables.push(native.tables.len() as u32);
                    }
                }
            }
            Payload::CustomSection(section)
                if section.name() == OBSOLETE_TRANSACTION_OBJECTS_CUSTOM_SECTION =>
            {
                obsolete_sections += 1;
            }
            _ => {}
        }
    }

    (obsolete_sections, native)
}

#[test]
fn rewrite_lowers_scalar_tmemory_roots_to_native_spaces() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $transfer (export "transfer") (param $root i64) (param $delta i32) (result i32)
            (local $base i32)
            (local $ptr i32)
            (local $next i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            local.set $base
            local.get $base
            i32.const 4
            i32.add
            local.tee $ptr
            i32.load
            local.get $delta
            i32.add
            local.tee $next
            local.get $ptr
            local.get $next
            i32.store
            local.get $next)
          (func $wrapper (export "wrapper") (param $root i64) (param $delta i32) (result i32)
            local.get $root
            local.get $delta
            call $transfer)
          (func $i64_transfer (export "i64_transfer") (param $root i64) (param $delta i64) (result i64)
            (local $ptr i32)
            (local $next i64)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            local.tee $ptr
            i64.load
            local.get $delta
            i64.add
            local.tee $next
            local.get $ptr
            local.get $next
            i64.store
            local.get $next)
          (func $f32_transfer (export "f32_transfer") (param $root i64) (param $delta f32) (result f32)
            (local $ptr i32)
            (local $next f32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            local.tee $ptr
            f32.load
            local.get $delta
            f32.add
            local.tee $next
            local.get $ptr
            local.get $next
            f32.store
            local.get $next)
          (func $f64_transfer (export "f64_transfer") (param $root i64) (param $delta f64) (result f64)
            (local $ptr i32)
            (local $next f64)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            local.tee $ptr
            f64.load
            local.get $delta
            f64.add
            local.tee $next
            local.get $ptr
            local.get $next
            f64.store
            local.get $next))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(
        report,
        RewriteReport {
            transaction_functions: 4,
            persistent_addr_markers: 4,
            i32_tloads: 1,
            i64_tloads: 1,
            f32_tloads: 1,
            f64_tloads: 1,
            i32_tstores: 1,
            i64_tstores: 1,
            f32_tstores: 1,
            f64_tstores: 1,
        }
    );
    assert!(!printed.contains("__twasm_persistent_addr_mut"));
    assert!(!printed.contains("__twasm_mark_transaction_func"));
    assert!(printed.contains("i32.tload"));
    assert!(printed.contains("i32.tstore"));
    assert!(printed.contains("i64.tload"));
    assert!(printed.contains("i64.tstore"));
    assert!(printed.contains("f32.tload"));
    assert!(printed.contains("f32.tstore"));
    assert!(printed.contains("f64.tload"));
    assert!(printed.contains("f64.tstore"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(
        metadata,
        TransactionObjects {
            memories: vec![0],
            globals: vec![],
            functions: vec![0, 2, 3, 4],
            tables: vec![],
        }
    );
}

#[test]
fn rewrite_lowers_narrow_i32_tmemory_ops() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (func (export "roundtrip") (param $root i64) (param $value i32) (result i32)
            local.get $root
            call $persistent_addr
            local.get $value
            i32.store8
            local.get $root
            call $persistent_addr
            i32.load8_u))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 2);
    assert_eq!(report.i32_tloads, 1);
    assert_eq!(report.i32_tstores, 1);
    assert!(printed.contains("i32.tstore8"));
    assert!(printed.contains("i32.tload8_u"));
}

#[test]
fn rewrite_lowers_mixed_memory_copy_to_tmemory_store_loop() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (data (i32.const 32) "hello")
          (func (export "copy")
            i64.const 256
            call $persistent_addr
            i32.const 32
            i32.const 5
            memory.copy))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tstores, 1);
    assert!(printed.contains("i32.load8_u"));
    assert!(printed.contains("i32.tstore8"));
    assert!(!printed.contains("memory.copy"));
}

#[test]
fn rewrite_lowers_memory_copy_when_destination_is_persistent_helper_result() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $alloc (result i32)
            i64.const 256
            call $persistent_addr)
          (func (export "copy") (param $src i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            call $alloc
            local.get $src
            i32.const 5
            memory.copy))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 0);
    assert_eq!(report.i32_tstores, 0);
    assert!(printed.contains("tmemory.copy"));
    assert!(
        !printed
            .lines()
            .any(|line| line.trim_start().starts_with("memory.copy"))
    );
}

#[test]
fn rewrite_propagates_persistent_helper_result_spilled_through_linear_memory() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $alloc (result i32)
            (local $sp i32)
            (local $ptr i32)
            i32.const 1024
            local.set $sp
            i64.const 256
            call $persistent_addr
            local.set $ptr
            local.get $sp
            local.get $ptr
            i32.store offset=36
            local.get $sp
            i32.load offset=36)
          (func (export "copy") (param $src i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            call $alloc
            local.get $src
            i32.const 5
            memory.copy))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 0);
    assert_eq!(report.i32_tstores, 0);
    assert!(printed.contains("tmemory.copy"));
}

#[test]
fn rewrite_clears_spilled_persistent_helper_result_after_definite_overwrite() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $alloc (result i32)
            (local $sp i32)
            i32.const 1024
            local.set $sp
            block
              local.get $sp
              i64.const 256
              call $persistent_addr
              i32.store offset=36
              br 0
            end
            local.get $sp
            i32.const 0
            i32.store offset=36
            local.get $sp
            i32.load offset=36)
          (func (export "copy") (param $src i32)
            call $mark_transaction
            call $alloc
            local.get $src
            i32.const 5
            memory.copy))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 0);
    assert_eq!(report.i32_tstores, 0);
    assert!(
        printed
            .lines()
            .any(|line| line.trim_start().starts_with("memory.copy"))
    );
}

#[test]
fn rewrite_infers_persistent_params_through_branchy_precheck_helper() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $runtime (param $dst i32) (param $src i32) (param $len i32) (param $count i32) (result i32)
            local.get $src
            i32.load
            drop
            i32.const 1)
          (func $precheck (param $dst i32) (param $src i32) (param $len i32) (param $align i32) (param $count i32)
            block
              block
                local.get $count
                br_if 0
                br 1
              end
              block
                local.get $src
                i32.const 0
                i32.eq
                br_if 0
              end
              local.get $dst
              local.get $src
              local.get $len
              local.get $count
              call $runtime
              drop
            end)
          (func (export "go") (param $root i64)
            call $mark_transaction
            i32.const 0
            local.get $root
            call $persistent_addr
            i32.const 4
            i32.const 1
            i32.const 1
            call $precheck))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(printed.contains("i32.tload"));
}

#[test]
fn rewrite_propagates_persistent_pointer_written_through_out_param() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $as_slice (param $out i32) (param $object i32)
            local.get $out
            local.get $object
            i32.load offset=4
            i32.store
            local.get $out
            local.get $object
            i32.load offset=8
            i32.store offset=4)
          (func (export "read_byte") (param $root i64) (result i32)
            (local $sp i32)
            (local $ptr i32)
            call $mark_transaction
            i32.const 1024
            local.set $sp
            local.get $sp
            local.get $root
            call $persistent_addr
            call $as_slice
            local.get $sp
            i32.load
            local.set $ptr
            local.get $ptr
            i32.load8_u))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 3);
    assert!(printed.contains("i32.tload offset=4"));
    assert!(printed.contains("i32.tload8_u"));
}

#[test]
fn rewrite_propagates_full_i32_load_results_as_persistent_pointers() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (func $field_ptr (param $root i64) (result i32)
            local.get $root
            call $persistent_addr
            i32.load)
          (func (export "read") (param $root i64) (result i32)
            local.get $root
            call $field_ptr
            i32.load))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 2);
    assert_eq!(printed.matches("i32.tload").count(), 2);
}

#[test]
fn rewrite_does_not_propagate_narrow_i32_load_results_as_persistent_pointers() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (func $byte_value (param $root i64) (result i32)
            local.get $root
            call $persistent_addr
            i32.load8_u)
          (func (export "read") (param $root i64) (result i32)
            local.get $root
            call $byte_value
            i32.load))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(printed.contains("i32.tload8_u"));
    assert!(printed.contains("i32.load"));
}

#[test]
fn rewrite_taints_marked_persistent_parameters() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func (export "update") (param $ptr i32) (param $delta i32) (result i32)
            (local $next i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            i32.load
            local.get $delta
            i32.add
            local.tee $next
            local.get $ptr
            local.get $next
            i32.store
            local.get $next))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 1);
    assert_eq!(report.persistent_addr_markers, 0);
    assert_eq!(report.i32_tloads, 1);
    assert_eq!(report.i32_tstores, 1);
    assert!(!printed.contains("__twasm_mark_persistent_arg"));
    assert!(printed.contains("i32.tload"));
    assert!(printed.contains("i32.tstore"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.memories, vec![0]);
    assert_eq!(metadata.functions, vec![0]);
}

#[test]
fn rewrite_taints_persistent_parameter_marker_spilled_through_local() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func (export "load") (param $ptr i32) (result i32)
            (local $marker i32)
            call $mark_transaction
            i32.const 0
            local.set $marker
            local.get $marker
            call $mark_persistent_arg
            local.get $ptr
            i32.load))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(!printed.contains("__twasm_mark_persistent_arg"));
    assert!(printed.contains("i32.tload"));
}

#[test]
fn rewrite_taints_dynamic_indexed_persistent_addresses() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func (export "transfer") (param $ptr i32) (param $index i32) (param $delta i64) (result i64)
            (local $slot i32)
            (local $next i64)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            local.get $index
            i32.const 1
            i32.and
            i32.const 3
            i32.shl
            i32.add
            local.tee $slot
            i64.load
            local.get $delta
            i64.add
            local.tee $next
            local.get $slot
            local.get $next
            i64.store
            local.get $next))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 1);
    assert_eq!(report.i64_tloads, 1);
    assert_eq!(report.i64_tstores, 1);
    assert!(printed.contains("i64.tload"));
    assert!(printed.contains("i64.tstore"));
}

#[test]
fn rewrite_allows_persistent_pointer_to_marked_transaction_callee() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $callee (param $ptr i32) (param $delta i32) (result i32)
            (local $next i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            i32.load
            local.get $delta
            i32.add
            local.tee $next
            local.get $ptr
            local.get $next
            i32.store
            local.get $next)
          (func $caller (export "caller") (param $root i64) (param $delta i32) (result i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            local.get $delta
            call $callee))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 2);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert_eq!(report.i32_tstores, 1);
    assert!(!printed.contains("__twasm_persistent_addr_mut"));
    assert!(!printed.contains("__twasm_mark_transaction_func"));
    assert!(!printed.contains("__twasm_mark_persistent_arg"));
    assert!(printed.contains("i32.tload"));
    assert!(printed.contains("i32.tstore"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.memories, vec![0]);
    assert_eq!(metadata.functions, vec![0, 1]);
}

#[test]
fn rewrite_propagates_persistent_pointer_return_from_marked_callee() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $identity (param $ptr i32) (result i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr)
          (func $caller (export "caller") (param $root i64) (result i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            call $identity
            i32.load))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 2);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(!printed.contains("twasm_intrinsics"));
    assert!(printed.contains("i32.tload"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.functions, vec![0, 1]);
}

#[test]
fn rewrite_propagates_forward_persistent_pointer_return_from_marked_callee() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $caller (export "caller") (param $root i64) (result i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            call $identity
            i32.load)
          (func $identity (param $ptr i32) (result i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 2);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(!printed.contains("twasm_intrinsics"));
    assert!(printed.contains("i32.tload"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.functions, vec![0, 1]);
}

#[test]
fn rewrite_propagates_persistent_allocator_wrapper_result_to_i64_store() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $persistent_alloc (param $size i32) (result i32)
            call $mark_transaction
            local.get $size
            i32.eqz
            if
              i32.const 0
              return
            end
            i64.const 131072
            call $persistent_addr)
          (func $rust_alloc (param $size i32) (result i32)
            local.get $size
            call $persistent_alloc)
          (func (export "init_box")
            call $mark_transaction
            i32.const 8
            call $rust_alloc
            i64.const 7
            i64.store))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 2);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i64_tstores, 1);
    assert!(printed.contains("i64.tstore"));
}

#[test]
fn rewrite_propagates_branchy_out_param_allocator_result_to_i64_store() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func $inner_alloc (param $out i32)
            block
              i32.const 0
              br_if 0
            end
            local.get $out
            i64.const 131072
            call $persistent_addr
            i32.store)
          (func $box_new_out (param $out i32)
            (local $sp i32)
            i32.const 1024
            local.set $sp
            local.get $sp
            call $inner_alloc
            local.get $out
            local.get $sp
            i32.load
            i32.store)
          (func (export "init_box")
            (local $sp i32)
            call $mark_transaction
            i32.const 2048
            local.set $sp
            local.get $sp
            call $box_new_out
            local.get $sp
            i32.load
            i64.const 7
            i64.store))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 1);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i64_tstores, 1);
    assert!(printed.contains("i64.tstore"));
}

#[test]
fn rewrite_propagates_nested_forward_persistent_pointer_return() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $root_ptr (param $root i64) (result i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            call $identity)
          (func $caller (export "caller") (param $root i64) (result i32)
            call $mark_transaction
            local.get $root
            call $root_ptr
            i32.load)
          (func $identity (param $ptr i32) (result i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 3);
    assert_eq!(report.persistent_addr_markers, 1);
    assert_eq!(report.i32_tloads, 1);
    assert!(!printed.contains("twasm_intrinsics"));
    assert!(printed.contains("i32.tload"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.functions, vec![0, 1, 2]);
}

#[test]
fn rewrite_merges_persistent_pointer_return_taint_from_multiple_return_sites() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $choose (param $ptr i32) (param $fallback i32) (param $pick_ptr i32) (result i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $pick_ptr
            if
              local.get $ptr
              return
            end
            local.get $fallback)
          (func $caller (export "caller") (param $ptr i32) (param $fallback i32) (param $pick_ptr i32) (result i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            local.get $fallback
            local.get $pick_ptr
            call $choose
            i32.load))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.transaction_functions, 2);
    assert_eq!(report.persistent_addr_markers, 0);
    assert_eq!(report.i32_tloads, 1);
    assert!(printed.contains("i32.tload"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert_eq!(metadata.functions, vec![0, 1]);
}

#[test]
fn rewrite_rejects_unknown_twasm_intrinsics_function_imports() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_extra" (func $extra))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (memory 1)
          (func (export "load") (param $root i64) (result i32)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            i32.load))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("unsupported twasm_intrinsics function import __twasm_extra"));
}

#[test]
fn rewrite_preserves_non_tainted_scalar_memory_ops() {
    let wat = r#"
        (module
          (memory 1)
          (func (export "plain") (param $ptr i32) (param $delta i32) (result i32)
            (local $next i32)
            local.get $ptr
            i32.load
            local.get $delta
            i32.add
            local.tee $next
            local.get $ptr
            local.get $next
            i32.store
            local.get $next))
    "#;

    let (output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report, RewriteReport::default());
    assert!(printed.contains("i32.load"));
    assert!(printed.contains("i32.store"));
    assert!(!printed.contains("i32.tload"));
    assert!(!printed.contains("i32.tstore"));

    let (section_count, metadata) = transaction_objects(&output);
    assert_eq!(section_count, 0);
    assert!(metadata.memories.is_empty());
    assert!(metadata.functions.is_empty());
}

#[test]
fn rewrite_rejects_persistent_pointer_escape_to_unrecognized_call() {
    let wat = r#"
        (module
          (import "env" "sink" (func $sink (param i32)))
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (func (export "escape") (param $root i64)
            local.get $root
            call $persistent_addr
            call $sink))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("persistent pointer escapes to unrecognized call in function index 2"));
}

#[test]
fn rewrite_allows_persistent_pointer_escape_to_immediate_trap_call() {
    let wat = r#"
        (module
          (import "env" "panic" (func $panic (param i32)))
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (memory 1)
          (func (export "trap") (param $root i64)
            local.get $root
            call $persistent_addr
            call $panic
            unreachable))
    "#;

    let (_output, report, printed) = rewrite_and_print(wat);

    assert_eq!(report.persistent_addr_markers, 1);
    assert!(printed.contains("call 0"));
    assert!(printed.contains("unreachable"));
}

#[test]
fn rewrite_rejects_persistent_pointer_forward_from_unmarked_caller() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $callee (param $ptr i32)
            call $mark_transaction
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            i32.load
            drop)
          (func $caller (export "caller") (param $root i64)
            local.get $root
            call $persistent_addr
            call $callee))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("persistent pointer escapes to unrecognized call in function index 4"));
}

#[test]
fn rewrite_rejects_persistent_pointer_forward_to_unmarked_callee() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $callee (param $ptr i32)
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            i32.load
            drop)
          (func $caller (export "caller") (param $root i64)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            call $callee))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("persistent pointer escapes to unrecognized call in function index 4"));
}

#[test]
fn rewrite_rejects_persistent_pointer_escape_to_call_indirect() {
    let wat = r#"
        (module
          (type $sink (func (param i32)))
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (table 1 funcref)
          (memory 1)
          (func (export "escape") (param $root i64)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            i32.const 0
            call_indirect (type $sink)))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("persistent pointer escapes to unrecognized call in function index 2"));
}

#[test]
fn rewrite_rejects_persistent_pointer_escape_to_return_call() {
    let wat = r#"
        (module
          (import "twasm_intrinsics" "__twasm_persistent_addr_mut" (func $persistent_addr (param i64) (result i32)))
          (import "twasm_intrinsics" "__twasm_mark_transaction_func" (func $mark_transaction))
          (import "twasm_intrinsics" "__twasm_mark_persistent_arg" (func $mark_persistent_arg (param i32)))
          (memory 1)
          (func $callee (param $ptr i32)
            i32.const 0
            call $mark_persistent_arg
            local.get $ptr
            i32.load
            drop)
          (func $caller (export "caller") (param $root i64)
            call $mark_transaction
            local.get $root
            call $persistent_addr
            return_call $callee))
    "#;

    let input = wat::parse_str(wat).expect("fixture parses");
    let err = rewrite_module(&input).unwrap_err().to_string();

    assert!(err.contains("persistent pointer escapes to unrecognized call in function index 4"));
}
