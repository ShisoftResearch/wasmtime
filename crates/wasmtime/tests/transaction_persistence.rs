use std::path::Path;

use tempfile::tempdir;

use wasmtime::_internal::transaction_persistence::{
    corrupt_first_log_crc, create_file_backed_region_image, publish_committed_global_object_root,
    publish_committed_struct_object, publish_committed_tmemory_update,
    reopen_and_recover_file_backed_region,
};
use wasmtime::{
    Config, Engine, ExternRef, Func, Global, GlobalType, Instance, Module, Mutability, Result,
    Store, ValType,
};

const OBJECT_VALUE_ABI_TAG_FUNCREF_FOR_TEST: u32 = 6;
const OBJECT_VALUE_ABI_TAG_EXTERNREF_FOR_TEST: u32 = 7;

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

#[test]
fn real_tfunc_promotes_exported_funcref_leaf_and_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-funcref-root.tmemory");
    let tx_log_path = dir.path().join("promote-funcref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (struct (field (mut funcref))))
              (func $target (export "target"))
              (elem declare func $target)
              (global $source (ref $s) (struct.new $s (ref.func $target)))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $source))))
            "#,
        )?,
    )?;

    let mut store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::create_file_backed_storage_for_test(
        &mut store,
        tmemory_path,
        tx_log_path.clone(),
        64,
    )?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let expected_module_fingerprint =
        wasmtime::_internal::transaction_persistence::module_fingerprint_for_test(&module)?;
    wasmtime::_internal::transaction_persistence::register_exported_durable_func_ref_for_test(
        &mut store, &instance, "target", 1,
    )?;

    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;
    publish.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(
        recovered_field_abi_parts(&recovered.object_winners[0].record_bytes),
        (
            OBJECT_VALUE_ABI_TAG_FUNCREF_FOR_TEST,
            expected_module_fingerprint,
            1_u64 << 32,
        )
    );

    Ok(())
}

#[test]
fn durable_funcref_registration_rejects_conflicting_identity() -> Result<()> {
    let engine = transaction_root_engine()?;
    let mut store = Store::new(&engine, ());
    let func = Func::wrap(&mut store, || {});

    wasmtime::_internal::transaction_persistence::register_durable_func_ref_for_test(
        &mut store, &func, 1, 2, 3,
    )?;
    wasmtime::_internal::transaction_persistence::register_durable_func_ref_for_test(
        &mut store, &func, 1, 2, 3,
    )?;

    let err = wasmtime::_internal::transaction_persistence::register_durable_func_ref_for_test(
        &mut store, &func, 1, 99, 3,
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("durable function reference identity registration conflict"),
        "{err:?}"
    );

    Ok(())
}

#[test]
fn exported_funcref_auto_identity_requires_retained_module_bytecode() -> Result<()> {
    let engine = Engine::default();
    let module = Module::new(&engine, r#"(module (func (export "target")))"#)?;

    let err = wasmtime::_internal::transaction_persistence::module_fingerprint_for_test(&module)
        .unwrap_err();
    assert!(
        err.to_string().contains("requires retained Wasm bytecode"),
        "{err:?}"
    );

    Ok(())
}

#[test]
fn real_tfunc_rejects_unregistered_funcref_leaf_before_commit() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("reject-funcref-root.tmemory");
    let tx_log_path = dir.path().join("reject-funcref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (struct (field (mut funcref))))
              (func $target)
              (elem declare func $target)
              (global $source (ref $s) (struct.new $s (ref.func $target)))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $source))))
            "#,
        )?,
    )?;

    let mut store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::create_file_backed_storage_for_test(
        &mut store,
        tmemory_path,
        tx_log_path,
        64,
    )?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;

    let err = publish.call(&mut store, ()).unwrap_err();
    let err = format!("{err:?}");
    assert!(
        err.contains("without registered durable function identity"),
        "{err}"
    );

    Ok(())
}

#[test]
fn real_tfunc_promotes_durable_externref_leaf_and_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("promote-externref-root.tmemory");
    let tx_log_path = dir.path().join("promote-externref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (import "" "source" (global $source externref))
              (type $s (struct (field (mut externref))))
              (global $boxed (ref $s) (struct.new $s (global.get $source)))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $boxed))))
            "#,
        )?,
    )?;

    let mut store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::create_file_backed_storage_for_test(
        &mut store,
        tmemory_path,
        tx_log_path.clone(),
        64,
    )?;
    let extern_ref = wasmtime::_internal::transaction_persistence::new_durable_extern_ref_for_test(
        &mut store,
        7,
        0x5555_6666_7777_8888,
        2,
    )?;
    let source_global = Global::new(
        &mut store,
        GlobalType::new(ValType::EXTERNREF, Mutability::Const),
        extern_ref.into(),
    )?;
    let instance = Instance::new(&mut store, &module, &[source_global.into()])?;

    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;
    publish.call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(
        recovered_field_abi_parts(&recovered.object_winners[0].record_bytes),
        (
            OBJECT_VALUE_ABI_TAG_EXTERNREF_FOR_TEST,
            0x5555_6666_7777_8888,
            7 | (2_u64 << 32),
        )
    );

    Ok(())
}

#[test]
fn real_tfunc_rejects_unregistered_externref_leaf_before_commit() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("reject-externref-root.tmemory");
    let tx_log_path = dir.path().join("reject-externref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (import "" "source" (global $source externref))
              (type $s (struct (field (mut externref))))
              (global $boxed (ref $s) (struct.new $s (global.get $source)))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (global.get $boxed))))
            "#,
        )?,
    )?;

    let mut store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::create_file_backed_storage_for_test(
        &mut store,
        tmemory_path,
        tx_log_path,
        64,
    )?;
    let extern_ref = ExternRef::new(&mut store, "durable-host-value")?;
    let source_global = Global::new(
        &mut store,
        GlobalType::new(ValType::EXTERNREF, Mutability::Const),
        extern_ref.into(),
    )?;
    let instance = Instance::new(&mut store, &module, &[source_global.into()])?;
    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;

    let err = publish.call(&mut store, ()).unwrap_err();
    let err = format!("{err:?}");
    assert!(
        err.contains("without embedded durable external identity"),
        "{err}"
    );

    Ok(())
}

fn recovered_field_abi_parts(record_bytes: &[u8]) -> (u32, u64, u64) {
    let field_offset = record_bytes
        .len()
        .checked_sub(20)
        .expect("recovered object record should contain one packed value slot");
    (
        u32::from_le_bytes(
            record_bytes[field_offset..field_offset + 4]
                .try_into()
                .unwrap(),
        ),
        u64::from_le_bytes(
            record_bytes[field_offset + 4..field_offset + 12]
                .try_into()
                .unwrap(),
        ),
        u64::from_le_bytes(
            record_bytes[field_offset + 12..field_offset + 20]
                .try_into()
                .unwrap(),
        ),
    )
}

fn transaction_root_engine() -> Result<Engine> {
    let mut config = Config::new();
    config
        .wasm_gc(true)
        .wasm_reference_types(true)
        .wasm_function_references(true)
        .wasm_tail_call(true)
        .guest_debug(true);
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
