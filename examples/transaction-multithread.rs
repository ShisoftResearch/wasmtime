//! Host-threaded transactional persistence prototype.
//!
//! This example uses one Wasmtime `Store` per host thread, with both stores
//! adopting the same shared transaction-region runtime and file-backed storage.
//!
//! You can execute this example with:
//!
//!     cargo run --example transaction-multithread

use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use tempfile::tempdir;
use wasmtime::_internal::transaction_persistence::{
    SharedTransactionRegionRuntimeForTest, TransactionPersistenceRecoveredRegion,
    adopt_shared_runtime_for_test, create_shared_file_backed_runtime_for_test,
    recover_file_backed_tmemory_for_test, reopen_and_recover_file_backed_region,
};
use wasmtime::{Config, Engine, Instance, Module, Result, Store};

fn main() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("transaction-multithread.tmemory");
    let tx_log_path = dir.path().join("transaction-multithread.txlog");

    let engine = transaction_engine()?;
    let module = Module::new(
        &engine,
        r#"
        (module
          (tmemory 1)
          (type $leaf (tstruct (field (mut i32))))
          (type $root (tstruct (field (mut (ref null $leaf)))))
          (tglobal $left (mut (ref null $root)) (ref.null $root))
          (tglobal $right (mut (ref null $root)) (ref.null $root))
          (tfunc (export "publish-left") (param $value i32)
            (i32.tstore (i32.const 0) (local.get $value))
            (drop (tmemory.size))
            (tglobal.set $left
              (tstruct.new $root
                (tstruct.new $leaf (local.get $value)))))
          (tfunc (export "publish-right") (param $value i32)
            (i32.tstore (i32.const 64) (local.get $value))
            (drop (tmemory.size))
            (tglobal.set $right
              (tstruct.new $root
                (tstruct.new $leaf (local.get $value))))))
        "#,
    )?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime)?;

    let start = Arc::new(Barrier::new(2));
    let left = spawn_publish(
        engine.clone(),
        module.clone(),
        runtime.clone(),
        start.clone(),
        "publish-left",
        11,
    );
    let right = spawn_publish(
        engine.clone(),
        module.clone(),
        runtime.clone(),
        start,
        "publish-right",
        22,
    );

    left.join().unwrap()?;
    right.join().unwrap()?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    let leaf_values = recovered_graph_leaf_values(&recovered)?;
    assert_eq!(leaf_values, vec![11, 22]);

    recover_file_backed_tmemory_for_test(&tx_log_path, &tmemory_path, 1, Some(1))?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &11u32.to_le_bytes())?;
    assert_tmemory_file_bytes(&tmemory_path, 64, &22u32.to_le_bytes())?;

    println!("recovered tmemory[0]=11, tmemory[64]=22");
    println!("recovered persistent object leaf values: {leaf_values:?}");
    Ok(())
}

fn transaction_engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_bulk_memory(true)
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true)
        .guest_debug(true);
    Engine::new(&config)
}

fn warm_shared_module(
    engine: &Engine,
    module: &Module,
    runtime: &SharedTransactionRegionRuntimeForTest,
) -> Result<()> {
    let mut store = shared_store(engine, runtime)?;
    Instance::new(&mut store, module, &[])?;
    Ok(())
}

fn spawn_publish(
    engine: Engine,
    module: Module,
    runtime: SharedTransactionRegionRuntimeForTest,
    start: Arc<Barrier>,
    name: &'static str,
    value: i32,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let mut store = shared_store(&engine, &runtime)?;
        let instance = Instance::new(&mut store, &module, &[])?;
        start.wait();
        instance
            .get_typed_func::<i32, ()>(&mut store, name)?
            .call(&mut store, value)
    })
}

fn shared_store(
    engine: &Engine,
    runtime: &SharedTransactionRegionRuntimeForTest,
) -> Result<Store<()>> {
    let mut store = Store::new(engine, ());
    adopt_shared_runtime_for_test(&mut store, runtime)?;
    Ok(store)
}

fn recovered_graph_leaf_values(
    recovered: &TransactionPersistenceRecoveredRegion,
) -> Result<Vec<i32>> {
    let mut values = recovered
        .root_object_ids
        .iter()
        .map(|&root_id| {
            let root = recovered
                .object_winners
                .iter()
                .find(|winner| winner.object_id == root_id)
                .expect("recovered root should have a corresponding object record");
            let leaf_id = recovered_object_ref_field(&root.record_bytes, 1, 0)?
                .expect("root field should point to a child object");
            let leaf = recovered
                .object_winners
                .iter()
                .find(|winner| winner.object_id == leaf_id)
                .expect("recovered leaf should have a corresponding object record");
            recovered_object_i32_field(&leaf.record_bytes, 1, 0)
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable();
    Ok(values)
}

fn recovered_object_ref_field(
    record_bytes: &[u8],
    field_count: usize,
    field_index: usize,
) -> Result<Option<u64>> {
    const OBJECT_VALUE_ABI_TAG_REF: u32 = 5;

    let slot = recovered_object_slot(record_bytes, field_count, field_index)?;
    let tag = u32::from_le_bytes(slot[0..4].try_into().unwrap());
    let low = u64::from_le_bytes(slot[4..12].try_into().unwrap());
    let high = u64::from_le_bytes(slot[12..20].try_into().unwrap());
    assert_eq!(tag, OBJECT_VALUE_ABI_TAG_REF);
    assert_eq!(high, 0);
    Ok(low.checked_sub(1))
}

fn recovered_object_i32_field(
    record_bytes: &[u8],
    field_count: usize,
    field_index: usize,
) -> Result<i32> {
    const OBJECT_VALUE_ABI_TAG_I32: u32 = 0;

    let slot = recovered_object_slot(record_bytes, field_count, field_index)?;
    let tag = u32::from_le_bytes(slot[0..4].try_into().unwrap());
    let low = u64::from_le_bytes(slot[4..12].try_into().unwrap());
    let high = u64::from_le_bytes(slot[12..20].try_into().unwrap());
    assert_eq!(tag, OBJECT_VALUE_ABI_TAG_I32);
    assert_eq!(high, 0);
    Ok(low as u32 as i32)
}

fn recovered_object_slot(
    record_bytes: &[u8],
    field_count: usize,
    field_index: usize,
) -> Result<&[u8]> {
    let slots_len = field_count
        .checked_mul(20)
        .expect("field slot byte overflow");
    let field_offset = record_bytes
        .len()
        .checked_sub(slots_len)
        .expect("recovered object record is shorter than its payload");
    let slot_offset = field_index
        .checked_mul(20)
        .expect("field slot offset overflow");
    let slot_end = slot_offset
        .checked_add(20)
        .expect("field slot end overflow");
    Ok(&record_bytes[field_offset + slot_offset..field_offset + slot_end])
}

fn assert_tmemory_file_bytes(tmemory_path: &Path, offset: usize, expected: &[u8]) -> Result<()> {
    let tmemory_bytes = std::fs::read(tmemory_path)?;
    assert_eq!(&tmemory_bytes[offset..offset + expected.len()], expected);
    Ok(())
}
