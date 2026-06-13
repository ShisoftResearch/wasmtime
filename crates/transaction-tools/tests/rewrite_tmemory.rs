use wasmparser::{BinaryReader, Parser, Payload};
use wasmtime_transaction_tools::{RewriteReport, rewrite_module};

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";

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
    let mut count = 0;
    let mut metadata = TransactionObjects::default();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("parse payload");
        if let Payload::CustomSection(section) = payload {
            if section.name() == TRANSACTION_OBJECTS_CUSTOM_SECTION {
                count += 1;
                metadata = parse_transaction_objects(section.data());
            }
        }
    }

    (count, metadata)
}

fn parse_transaction_objects(data: &[u8]) -> TransactionObjects {
    let mut reader = BinaryReader::new(data, 0);
    let version = reader.read_u8().expect("version");
    assert_eq!(version, 1);

    let memories = read_index_vec(&mut reader);
    let globals = read_index_vec(&mut reader);
    let functions = read_index_vec(&mut reader);
    let tables = if reader.eof() {
        Vec::new()
    } else {
        read_index_vec(&mut reader)
    };

    assert!(reader.eof(), "unexpected trailing metadata bytes");
    TransactionObjects {
        memories,
        globals,
        functions,
        tables,
    }
}

fn read_index_vec(reader: &mut BinaryReader<'_>) -> Vec<u32> {
    let len = reader.read_var_u32().expect("vector length");
    (0..len)
        .map(|_| reader.read_var_u32().expect("vector item"))
        .collect()
}

#[test]
fn rewrite_lowers_scalar_tmemory_roots_and_emits_transaction_metadata() {
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
    assert_eq!(section_count, 1);
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
    assert_eq!(section_count, 1);
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
    assert_eq!(section_count, 1);
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
    assert_eq!(section_count, 1);
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
    assert_eq!(section_count, 1);
    assert_eq!(metadata.functions, vec![0, 1]);
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
    assert_eq!(section_count, 1);
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
    assert_eq!(section_count, 1);
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
