use std::path::Path;

use tempfile::tempdir;

use wasmtime::_internal::transaction_persistence::{
    corrupt_first_log_crc, create_file_backed_region_image, publish_committed_global_object_root,
    publish_committed_struct_object, publish_committed_tmemory_update,
    reopen_and_recover_file_backed_region,
};
use wasmtime::{Config, Engine, Instance, Module, Result, Store};

#[test]
fn file_backed_region_recovers_committed_tmemory_update() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("tmemory-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_tmemory_update(&path, 1, 0x1000_0000_0000_0001, 1, &[1, 2, 3, 4]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.winners.len(), 1);
    assert_eq!(recovered.winners[0].logical_id, 0x1000_0000_0000_0001);
    assert_eq!(recovered.winners[0].version, 1);
}

#[test]
fn file_backed_region_rejects_corrupted_log_crc() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("corrupt-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_tmemory_update(&path, 1, 0x1000_0000_0000_0002, 1, &[9, 8, 7, 6]).unwrap();
    corrupt_first_log_crc(&path).unwrap();

    let err = reopen_and_recover_file_backed_region(&path).unwrap_err();
    assert!(err.to_string().contains("corrupt log entry"));
}

#[test]
fn file_backed_region_recovers_committed_struct_object() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[9, 8, 7]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 41);
    assert_eq!(recovered.object_winners[0].version, 1);
}

#[test]
fn file_backed_region_recovers_object_rooted_by_global() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rooted-object-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[1, 2, 3]).unwrap();
    publish_committed_global_object_root(&path, 2, 41).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 41);
    assert_eq!(recovered.root_object_ids, vec![41]);
}

#[test]
fn real_tfunc_root_publication_reopens_without_version_collision() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("real-root.tmemory");
    let tx_log_path = dir.path().join("real-root.txlog");
    let engine = transaction_root_engine()?;
    let module = persistent_root_module(&engine)?;

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, true)?;
    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids, vec![0]);

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, false)?;
    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids, vec![0]);
    let root_winner = recovered
        .winners
        .iter()
        .find(|winner| winner.logical_id >> 60 == 3)
        .expect("expected recovered tglobal root winner");
    assert_eq!(root_winner.version, 2);

    Ok(())
}

#[test]
fn real_tfunc_promotes_tstruct_root_and_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-root.tmemory");
    let tx_log_path = dir.path().join("promote-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (tstruct.new $s (i32.const 77)))))
            "#,
        )?,
    )?;

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, true)?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(
        recovered.root_object_ids[0],
        recovered.object_winners[0].object_id
    );

    Ok(())
}

#[test]
fn real_tfunc_promotes_ordinary_array_root_and_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-array-root.tmemory");
    let tx_log_path = dir.path().join("promote-array-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $a (array (mut i32)))
              (global $source (ref $a) (array.new $a (i32.const 9) (i32.const 3)))
              (tglobal $root (mut (ref null $a)) (ref.null $a))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $source))))
            "#,
        )?,
    )?;

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, true)?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(
        recovered.root_object_ids[0],
        recovered.object_winners[0].object_id
    );

    Ok(())
}

#[test]
fn real_tfunc_promotes_ordinary_ref_array_with_i31_leaf_and_child_object() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-ref-array-root.tmemory");
    let tx_log_path = dir.path().join("promote-ref-array-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (struct (field i32)))
              (type $a (array (mut eqref)))
              (tglobal $root (mut (ref null $a)) (ref.null $a))
              (tfunc (export "publish")
                (tglobal.set $root
                  (array.new_fixed $a 2
                    (ref.i31 (i32.const 7))
                    (struct.new $s (i32.const 9))))))
            "#,
        )?,
    )?;

    call_publish_root(&engine, &module, &tmemory_path, &tx_log_path, true)?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 2);
    assert!(
        recovered
            .object_winners
            .iter()
            .any(|winner| winner.object_id == recovered.root_object_ids[0])
    );

    Ok(())
}

fn transaction_root_engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true);
    Engine::new(&config)
}

fn persistent_root_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (global $source (ref $s) (tstruct.new $s (i32.const 41)))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $source))))
            "#,
        )?,
    )
}

fn call_publish_root(
    engine: &Engine,
    module: &Module,
    tmemory_path: &Path,
    tx_log_path: &Path,
    create: bool,
) -> Result<()> {
    let mut store = Store::new(engine, ());
    if create {
        wasmtime::_internal::transaction_persistence::create_file_backed_storage_for_test(
            &mut store,
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
            64,
        )?;
    } else {
        wasmtime::_internal::transaction_persistence::open_file_backed_storage_for_test(
            &mut store,
            tmemory_path.to_path_buf(),
            tx_log_path.to_path_buf(),
        )?;
    }
    let instance = Instance::new(&mut store, module, &[])?;
    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;
    publish.call(&mut store, ())
}
