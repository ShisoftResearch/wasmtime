use std::path::Path;

use tempfile::tempdir;

use wasmtime::_internal::transaction_persistence::{
    SharedTransactionRegionRuntimeForTest, adopt_shared_runtime_for_test,
    bump_first_object_data_block_generation_for_test, corrupt_first_log_crc,
    create_file_backed_region_image, create_shared_file_backed_runtime_for_test,
    publish_committed_global_object_root, publish_committed_struct_object,
    publish_committed_tmemory_undo_for_test, publish_committed_tmemory_update,
    reopen_and_recover_file_backed_region, retire_completed_linear_undo_chunks_for_test,
    retire_unreachable_object_chunks_for_test,
};
use wasmtime::{
    Config, Engine, ExternRef, Func, Global, GlobalType, Instance, Module, Mutability, Result,
    Rooted, Store, ValType,
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
fn file_backed_recovery_ignores_stale_generation_object_publication() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("stale-generation-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[9, 8, 7]).unwrap();
    bump_first_object_data_block_generation_for_test(&path).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();

    assert!(recovered.winners.is_empty());
    assert!(recovered.object_winners.is_empty());
}

#[test]
fn file_backed_object_chunk_reuse_does_not_resurrect_old_object() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("object-reuse-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_struct_object(&path, 1, 41, 1, 12, &[1, 1, 1]).unwrap();

    retire_unreachable_object_chunks_for_test(&path, &[41]).unwrap();
    publish_committed_struct_object(&path, 2, 42, 1, 12, &[2, 2, 2]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();
    let object_ids = recovered
        .object_winners
        .iter()
        .map(|winner| winner.object_id)
        .collect::<Vec<_>>();

    assert_eq!(object_ids, vec![42]);
}

#[test]
fn file_backed_linear_undo_chunk_reuse_does_not_roll_back_committed_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("undo-reuse-region.bin");

    create_file_backed_region_image(&path, 128).unwrap();
    publish_committed_tmemory_undo_for_test(&path, 1, 0x1000_0000_0000_0001, 1, &[1, 1, 1, 1])
        .unwrap();
    retire_completed_linear_undo_chunks_for_test(&path).unwrap();
    publish_committed_struct_object(&path, 2, 42, 1, 12, &[2, 2, 2]).unwrap();

    let recovered = reopen_and_recover_file_backed_region(&path).unwrap();

    assert!(recovered.tmemory_undo_rollbacks.is_empty());
    assert_eq!(recovered.object_winners.len(), 1);
    assert_eq!(recovered.object_winners[0].object_id, 42);
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

    let mut restart_store = Store::new(&engine, ());
    let restart_instance = Instance::new(&mut restart_store, &module, &[])?;
    assert!(
        wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
            &mut restart_store,
            expected_module_fingerprint,
            0,
            1,
        )?
        .is_none()
    );
    let restart_target =
        wasmtime::_internal::transaction_persistence::register_exported_durable_func_ref_for_test(
            &mut restart_store,
            &restart_instance,
            "target",
            1,
        )?;
    let resolved = wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
        &mut restart_store,
        expected_module_fingerprint,
        0,
        1,
    )?
    .expect("restarted store should resolve registered durable function identity");
    assert_eq!(
        resolved.to_raw(&mut restart_store),
        restart_target.to_raw(&mut restart_store)
    );

    Ok(())
}

#[test]
fn real_tstruct_registered_funcref_leaf_roundtrips_through_get() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("roundtrip-funcref-root.tmemory");
    let tx_log_path = dir.path().join("roundtrip-funcref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (type $s (tstruct (field (mut funcref))))
              (func $target (export "target"))
              (elem declare func $target)
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (tstruct.new $s (ref.func $target))))
              (tfunc (export "leaf-is-null") (result i32)
                (ref.is_null
                  (tstruct.get $s 0
                    (tref.cast_read (ref.as_non_null (tglobal.get $root)))))))
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
    wasmtime::_internal::transaction_persistence::register_exported_durable_func_ref_for_test(
        &mut store, &instance, "target", 1,
    )?;

    let publish = instance.get_typed_func::<(), ()>(&mut store, "publish")?;
    publish.call(&mut store, ())?;
    let leaf_is_null = instance.get_typed_func::<(), i32>(&mut store, "leaf-is-null")?;
    assert_eq!(leaf_is_null.call(&mut store, ())?, 0);

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
fn durable_funcref_registration_rejects_ambiguous_identity() -> Result<()> {
    let engine = transaction_root_engine()?;
    let module = Module::new(&engine, r#"(module (func (export "target")))"#)?;
    let mut store = Store::new(&engine, ());
    let first = Instance::new(&mut store, &module, &[])?;
    let second = Instance::new(&mut store, &module, &[])?;

    wasmtime::_internal::transaction_persistence::register_exported_durable_func_ref_for_test(
        &mut store, &first, "target", 1,
    )?;
    let err =
        wasmtime::_internal::transaction_persistence::register_exported_durable_func_ref_for_test(
            &mut store, &second, "target", 1,
        )
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("resolves to multiple function references"),
        "{err:?}"
    );

    Ok(())
}

#[test]
fn durable_funcref_bulk_export_registration_resolves_recovered_identities() -> Result<()> {
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        r#"
        (module
          (func $first (export "first"))
          (func $second (export "second"))
          (global (export "not-a-func") i32 (i32.const 7)))
        "#,
    )?;
    let expected_module_fingerprint =
        wasmtime::_internal::transaction_persistence::module_fingerprint_for_test(&module)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;

    let registered =
        wasmtime::_internal::transaction_persistence::register_exported_durable_func_refs_for_test(
            &mut store, &instance, 1,
        )?;
    let registered_names = registered
        .iter()
        .map(|entry| entry.export_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(registered_names, ["first", "second"]);

    for entry in registered {
        let resolved =
            wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
                &mut store,
                expected_module_fingerprint,
                entry.function_index,
                1,
            )?
            .expect("bulk-registered durable function identity should resolve");
        assert_eq!(resolved.to_raw(&mut store), entry.func.to_raw(&mut store));
    }

    assert!(
        wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
            &mut store,
            expected_module_fingerprint,
            2,
            1,
        )?
        .is_none()
    );

    Ok(())
}

#[test]
fn durable_funcref_bulk_registration_ignores_non_function_exports_without_bytecode() -> Result<()> {
    let engine = Engine::default();
    let module = Module::new(
        &engine,
        r#"(module (global (export "not-a-func") i32 (i32.const 7)))"#,
    )?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;

    let registered =
        wasmtime::_internal::transaction_persistence::register_exported_durable_func_refs_for_test(
            &mut store, &instance, 1,
        )?;
    assert!(registered.is_empty());

    Ok(())
}

#[test]
fn durable_funcref_bulk_registration_skips_imported_function_exports() -> Result<()> {
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        r#"
        (module
          (import "" "host" (func $host))
          (func $local (export "local"))
          (export "host-reexport" (func $host)))
        "#,
    )?;
    let expected_module_fingerprint =
        wasmtime::_internal::transaction_persistence::module_fingerprint_for_test(&module)?;
    let mut store = Store::new(&engine, ());
    let host = Func::wrap(&mut store, || {});
    let instance = Instance::new(&mut store, &module, &[host.into()])?;

    let registered =
        wasmtime::_internal::transaction_persistence::register_exported_durable_func_refs_for_test(
            &mut store, &instance, 1,
        )?;
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].export_name, "local");
    assert_eq!(registered[0].function_index, 1);
    assert!(
        wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
            &mut store,
            expected_module_fingerprint,
            0,
            1,
        )?
        .is_none()
    );
    let resolved = wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
        &mut store,
        expected_module_fingerprint,
        1,
        1,
    )?
    .expect("defined exported function should be registered");
    assert_eq!(
        resolved.to_raw(&mut store),
        registered[0].func.to_raw(&mut store)
    );

    Ok(())
}

#[test]
fn durable_funcref_module_registration_covers_non_exported_ref_func_targets() -> Result<()> {
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        r#"
        (module
          (func $hidden)
          (func $plain)
          (elem declare func $hidden)
          (func $make (export "make") (result funcref)
            ref.func $hidden))
        "#,
    )?;
    let expected_module_fingerprint =
        wasmtime::_internal::transaction_persistence::module_fingerprint_for_test(&module)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;

    let registered = wasmtime::_internal::transaction_persistence::register_module_defined_durable_func_refs_for_test(
        &mut store,
        &instance,
    )?;
    let registered_indices = registered
        .iter()
        .map(|entry| entry.function_index)
        .collect::<Vec<_>>();
    assert!(registered_indices.contains(&0));
    assert!(registered_indices.contains(&2));

    for function_index in [0, 2] {
        assert!(
            wasmtime::_internal::transaction_persistence::resolve_durable_func_ref_for_test(
                &mut store,
                expected_module_fingerprint,
                function_index,
                5,
            )?
            .is_some()
        );
    }

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
fn real_tstruct_registered_externref_leaf_roundtrips_through_get() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("roundtrip-externref-root.tmemory");
    let tx_log_path = dir.path().join("roundtrip-externref-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (import "" "source" (global $source externref))
              (type $s (tstruct (field (mut externref))))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root (tstruct.new $s (global.get $source))))
              (tfunc (export "leaf") (result externref)
                (tstruct.get $s 0
                  (tref.cast_read (ref.as_non_null (tglobal.get $root))))))
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
    let extern_ref = wasmtime::_internal::transaction_persistence::new_durable_extern_ref_for_test(
        &mut store,
        9,
        0x1234_5678_90ab_cdef,
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
    let leaf = instance.get_typed_func::<(), Option<Rooted<ExternRef>>>(&mut store, "leaf")?;
    let returned = leaf
        .call(&mut store, ())?
        .expect("registered durable externref should roundtrip");
    assert_eq!(returned.to_raw(&mut store)?, extern_ref.to_raw(&mut store)?);

    Ok(())
}

#[test]
fn real_tstruct_registered_externref_as_anyref_roundtrips_through_get() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("roundtrip-extern-any-root.tmemory");
    let tx_log_path = dir.path().join("roundtrip-extern-any-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (import "" "source" (global $source externref))
              (type $s (tstruct (field (mut anyref))))
              (tglobal $root (mut (ref null $s)) (ref.null $s))
              (tfunc (export "publish")
                (tglobal.set $root
                  (tstruct.new $s (any.convert_extern (global.get $source)))))
              (tfunc (export "leaf") (result externref)
                (extern.convert_any
                  (tstruct.get $s 0
                    (tref.cast_read (ref.as_non_null (tglobal.get $root)))))))
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
    let extern_ref = wasmtime::_internal::transaction_persistence::new_durable_extern_ref_for_test(
        &mut store,
        10,
        0x2234_5678_90ab_cdef,
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
    let leaf = instance.get_typed_func::<(), Option<Rooted<ExternRef>>>(&mut store, "leaf")?;
    let returned = leaf
        .call(&mut store, ())?
        .expect("registered durable externref stored as anyref should roundtrip");
    assert_eq!(returned.to_raw(&mut store)?, extern_ref.to_raw(&mut store)?);

    Ok(())
}

#[test]
fn real_tarray_registered_externref_leaf_roundtrips_through_get() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("roundtrip-externref-array-root.tmemory");
    let tx_log_path = dir.path().join("roundtrip-externref-array-root.txlog");
    let engine = transaction_root_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (import "" "source" (global $source externref))
              (type $a (tarray (mut externref)))
              (tglobal $root (mut (ref null $a)) (ref.null $a))
              (tfunc (export "publish")
                (tglobal.set $root
                  (tarray.new $a (global.get $source) (i32.const 2))))
              (tfunc (export "leaf") (result externref)
                (tarray.get $a
                  (tref.cast_read (ref.as_non_null (tglobal.get $root)))
                  (i32.const 1))))
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
    let extern_ref = wasmtime::_internal::transaction_persistence::new_durable_extern_ref_for_test(
        &mut store,
        11,
        0x3234_5678_90ab_cdef,
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
    let leaf = instance.get_typed_func::<(), Option<Rooted<ExternRef>>>(&mut store, "leaf")?;
    let returned = leaf
        .call(&mut store, ())?
        .expect("registered durable externref array element should roundtrip");
    assert_eq!(returned.to_raw(&mut store)?, extern_ref.to_raw(&mut store)?);

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

#[test]
fn shared_runtime_two_store_object_graphs_recover_both_roots() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("multi-store-objects.tmemory");
    let tx_log_path = dir.path().join("multi-store-objects.txlog");
    let engine = transaction_persistence_engine()?;
    let module = two_root_object_graph_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    let mut left_store = shared_store(&engine, &runtime)?;
    let left_instance = Instance::new(&mut left_store, &module, &[])?;
    left_instance
        .get_typed_func::<i32, ()>(&mut left_store, "publish-left")?
        .call(&mut left_store, 11)?;

    let mut right_store = shared_store(&engine, &runtime)?;
    let right_instance = Instance::new(&mut right_store, &module, &[])?;
    right_instance
        .get_typed_func::<i32, ()>(&mut right_store, "publish-right")?
        .call(&mut right_store, 22)?;

    let mut reopened_store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::open_file_backed_storage_for_test(
        &mut reopened_store,
        tmemory_path,
        tx_log_path.clone(),
    )?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 2);
    assert_eq!(recovered.object_winners.len(), 4);
    assert_eq!(recovered_graph_leaf_values(&recovered)?, vec![11, 22]);

    Ok(())
}

#[test]
fn shared_runtime_same_object_conflict_recovers_only_committed_version() -> Result<()> {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("multi-store-conflict.tmemory");
    let tx_log_path = dir.path().join("multi-store-conflict.txlog");
    let engine = transaction_persistence_engine()?;
    let module = blocking_single_root_object_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    let gate = Arc::new((Mutex::new(false), Condvar::new()));

    let first_engine = engine.clone();
    let first_module = module.clone();
    let first_runtime = runtime.clone();
    let first_gate = gate.clone();
    let first = thread::spawn(move || -> Result<()> {
        let mut store = shared_store(&first_engine, &first_runtime)?;
        let gate = first_gate.clone();
        let pause = Func::wrap(&mut store, move || {
            let (lock, ready) = &*gate;
            let mut released = lock.lock().unwrap();
            *released = true;
            ready.notify_one();
            while *released {
                released = ready.wait(released).unwrap();
            }
        });
        let instance = Instance::new(&mut store, &first_module, &[pause.into()])?;
        instance
            .get_typed_func::<i32, ()>(&mut store, "publish")?
            .call(&mut store, 11)?;
        Ok(())
    });

    let (lock, ready) = &*gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        released = ready.wait(released).unwrap();
    }

    let mut second_store = shared_store(&engine, &runtime)?;
    let second_pause = Func::wrap(&mut second_store, || {});
    let second_instance = Instance::new(&mut second_store, &module, &[second_pause.into()])?;
    let err = second_instance
        .get_typed_func::<i32, ()>(&mut second_store, "publish")?
        .call(&mut second_store, 22)
        .unwrap_err();
    let err = format!("{err:?}");
    assert!(err.contains("transaction write conflict"), "{err}");

    *released = false;
    ready.notify_one();
    drop(released);
    first.join().unwrap()?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 2);
    assert_eq!(recovered_graph_leaf_values(&recovered)?, vec![11]);

    Ok(())
}

#[test]
fn shared_runtime_threaded_tmemory_commits_recover_together() -> Result<()> {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-tmemory.tmemory");
    let tx_log_path = dir.path().join("threaded-tmemory.txlog");
    let engine = transaction_persistence_engine()?;
    let module = threaded_tmemory_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime, 0)?;

    let start = Arc::new(Barrier::new(2));
    let left = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let start = start.clone();
        thread::spawn(move || -> Result<()> {
            start.wait();
            call_shared_tfunc_i32(&engine, &module, &runtime, "write-left", 0x1122_3344)
        })
    };
    let right = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        thread::spawn(move || -> Result<()> {
            start.wait();
            call_shared_tfunc_i32(&engine, &module, &runtime, "write-right", 0x5566_7788)
        })
    };

    left.join().unwrap()?;
    right.join().unwrap()?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &0x1122_3344u32.to_le_bytes())?;
    assert_tmemory_file_bytes(&tmemory_path, 64, &0x5566_7788u32.to_le_bytes())?;

    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
#[test]
fn mvcc_file_backed_lp_pre_lp_growth_tail_failure_does_not_leak() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("mvcc-growth-failure.tmemory");
    let tx_log_path = dir.path().join("mvcc-growth-failure.txlog");
    let engine = transaction_persistence_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (tmemory $m 1 2)
              (tfunc (export "grow-write")
                (drop (tmemory.grow $m (i32.const 1)))
                (i32.tstore $m (i32.const 65536) (i32.const 1144201745)))
              (tfunc (export "grow-read") (result i32)
                (drop (tmemory.grow $m (i32.const 1)))
                (i32.tload $m (i32.const 65536))))
            "#,
        )?,
    )?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;
    let mut store = shared_store(&engine, &runtime)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let grow_write = instance.get_typed_func::<(), ()>(&mut store, "grow-write")?;
    let grow_read = instance.get_typed_func::<(), i32>(&mut store, "grow-read")?;

    wasmtime::_internal::transaction_persistence::fail_next_commit_before_lp_for_test(&mut store);
    let error = grow_write.call(&mut store, ()).unwrap_err();
    assert!(
        format!("{error:#}").contains("transaction test failure before commit LP"),
        "{error:#}"
    );
    assert_eq!(grow_read.call(&mut store, ())?, 0);

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.committed_tmemory_pages, Some(2));
    assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        2,
        Some(2),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 65536, &[0, 0, 0, 0])?;

    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
#[test]
fn mvcc_file_backed_lp_post_lp_growth_tail_recovers() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("mvcc-growth-committed.tmemory");
    let tx_log_path = dir.path().join("mvcc-growth-committed.txlog");
    let engine = transaction_persistence_engine()?;
    let module = Module::new(
        &engine,
        wat::parse_str(
            r#"
            (module
              (tmemory $m 1 2)
              (tfunc (export "grow-write")
                (drop (tmemory.grow $m (i32.const 1)))
                (i32.tstore $m (i32.const 65536) (i32.const 1144201745))))
            "#,
        )?,
    )?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;
    let mut store = shared_store(&engine, &runtime)?;
    let instance = Instance::new(&mut store, &module, &[])?;

    instance
        .get_typed_func::<(), ()>(&mut store, "grow-write")?
        .call(&mut store, ())?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.committed_tmemory_pages, Some(2));
    assert!(recovered.tmemory_undo_rollbacks.is_empty());
    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        2,
        Some(2),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 65536, &0x4433_2211u32.to_le_bytes())?;

    Ok(())
}

#[test]
fn shared_runtime_threaded_object_graphs_recover_both_roots() -> Result<()> {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-objects.tmemory");
    let tx_log_path = dir.path().join("threaded-objects.txlog");
    let engine = transaction_persistence_engine()?;
    let module = two_root_object_graph_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    let start = Arc::new(Barrier::new(2));
    let left = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let start = start.clone();
        thread::spawn(move || -> Result<()> {
            start.wait();
            call_shared_tfunc_i32(&engine, &module, &runtime, "publish-left", 11)
        })
    };
    let right = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        thread::spawn(move || -> Result<()> {
            start.wait();
            call_shared_tfunc_i32(&engine, &module, &runtime, "publish-right", 22)
        })
    };

    left.join().unwrap()?;
    right.join().unwrap()?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 2);
    assert_eq!(recovered.object_winners.len(), 4);
    assert_eq!(recovered_graph_leaf_values(&recovered)?, vec![11, 22]);

    Ok(())
}

#[test]
fn shared_runtime_threaded_tmemory_conflict_recovers_only_committed_version() -> Result<()> {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-tmemory-conflict.tmemory");
    let tx_log_path = dir.path().join("threaded-tmemory-conflict.txlog");
    let engine = transaction_persistence_engine()?;
    let module = blocking_tmemory_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime, 1)?;

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let first = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let gate = gate.clone();
        thread::spawn(move || -> Result<()> {
            let mut store = shared_store(&engine, &runtime)?;
            let gate = gate.clone();
            let pause = Func::wrap(&mut store, move || {
                let (lock, ready) = &*gate;
                let mut released = lock.lock().unwrap();
                *released = true;
                ready.notify_one();
                while *released {
                    released = ready.wait(released).unwrap();
                }
            });
            let instance = Instance::new(&mut store, &module, &[pause.into()])?;
            instance
                .get_typed_func::<i32, ()>(&mut store, "write")?
                .call(&mut store, 0x1122_3344)?;
            Ok(())
        })
    };

    let (lock, ready) = &*gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        released = ready.wait(released).unwrap();
    }

    let mut second_store = shared_store(&engine, &runtime)?;
    let second_pause = Func::wrap(&mut second_store, || {});
    let second_instance = Instance::new(&mut second_store, &module, &[second_pause.into()])?;
    let err = second_instance
        .get_typed_func::<i32, ()>(&mut second_store, "write")?
        .call(&mut second_store, 0x5566_7788)
        .unwrap_err();
    let err = format!("{err:?}");
    assert_transaction_conflict(&err);

    *released = false;
    ready.notify_one();
    drop(released);
    first.join().unwrap()?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &0x1122_3344u32.to_le_bytes())?;

    Ok(())
}

#[test]
#[cfg(feature = "transaction-cc-nowait-abort")]
fn shared_runtime_nowait_abort_does_not_preempt_existing_owner() -> Result<()> {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-nowait-owner.tmemory");
    let tx_log_path = dir.path().join("threaded-nowait-owner.txlog");
    let engine = transaction_persistence_engine()?;
    let module = blocking_tmemory_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime, 1)?;

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let first = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let gate = gate.clone();
        thread::spawn(move || -> Result<()> {
            let mut store = shared_store(&engine, &runtime)?;
            let gate = gate.clone();
            let pause = Func::wrap(&mut store, move || {
                let (lock, ready) = &*gate;
                let mut released = lock.lock().unwrap();
                *released = true;
                ready.notify_one();
                while *released {
                    released = ready.wait(released).unwrap();
                }
            });
            let instance = Instance::new(&mut store, &module, &[pause.into()])?;
            instance
                .get_typed_func::<i32, ()>(&mut store, "write")?
                .call(&mut store, 0x1122_3344)?;
            Ok(())
        })
    };

    let (lock, ready) = &*gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        released = ready.wait(released).unwrap();
    }

    let mut second_store = shared_store(&engine, &runtime)?;
    let second_pause = Func::wrap(&mut second_store, || {});
    let second_instance = Instance::new(&mut second_store, &module, &[second_pause.into()])?;
    let err = second_instance
        .get_typed_func::<i32, ()>(&mut second_store, "write")?
        .call(&mut second_store, 0x5566_7788)
        .unwrap_err();
    let err = format!("{err:?}");
    assert_transaction_conflict(&err);

    *released = false;
    ready.notify_one();
    drop(released);
    first.join().unwrap()?;

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &0x1122_3344u32.to_le_bytes())?;

    Ok(())
}

#[test]
fn shared_runtime_threaded_mixed_conflict_recovers_only_committed_version() -> Result<()> {
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;

    let dir = tempdir()?;
    let tmemory_path = dir.path().join("threaded-mixed-conflict.tmemory");
    let tx_log_path = dir.path().join("threaded-mixed-conflict.txlog");
    let engine = transaction_persistence_engine()?;
    let module = blocking_mixed_tmemory_object_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    warm_shared_module(&engine, &module, &runtime, 1)?;

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let first = {
        let engine = engine.clone();
        let module = module.clone();
        let runtime = runtime.clone();
        let gate = gate.clone();
        thread::spawn(move || -> Result<()> {
            let mut store = shared_store(&engine, &runtime)?;
            let gate = gate.clone();
            let pause = Func::wrap(&mut store, move || {
                let (lock, ready) = &*gate;
                let mut released = lock.lock().unwrap();
                *released = true;
                ready.notify_one();
                while *released {
                    released = ready.wait(released).unwrap();
                }
            });
            let instance = Instance::new(&mut store, &module, &[pause.into()])?;
            instance
                .get_typed_func::<i32, ()>(&mut store, "publish")?
                .call(&mut store, 11)?;
            Ok(())
        })
    };

    let (lock, ready) = &*gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        released = ready.wait(released).unwrap();
    }

    let mut second_store = shared_store(&engine, &runtime)?;
    let second_pause = Func::wrap(&mut second_store, || {});
    let second_instance = Instance::new(&mut second_store, &module, &[second_pause.into()])?;
    let err = second_instance
        .get_typed_func::<i32, ()>(&mut second_store, "publish")?
        .call(&mut second_store, 22)
        .unwrap_err();
    let err = format!("{err:?}");
    assert_transaction_conflict(&err);

    *released = false;
    ready.notify_one();
    drop(released);
    first.join().unwrap()?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 1);
    assert_eq!(recovered.object_winners.len(), 2);
    assert_eq!(recovered_graph_leaf_values(&recovered)?, vec![11]);

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &11u32.to_le_bytes())?;

    Ok(())
}

#[test]
fn shared_runtime_mixed_object_and_tmemory_commits_recover_together() -> Result<()> {
    let dir = tempdir()?;
    let tmemory_path = dir.path().join("multi-store-mixed.tmemory");
    let tx_log_path = dir.path().join("multi-store-mixed.txlog");
    let engine = transaction_persistence_engine()?;
    let module = mixed_tmemory_object_graph_module(&engine)?;
    let runtime =
        create_shared_file_backed_runtime_for_test(tmemory_path.clone(), tx_log_path.clone(), 64)?;

    let mut left_store = shared_store(&engine, &runtime)?;
    let left_instance = Instance::new(&mut left_store, &module, &[])?;
    left_instance
        .get_typed_func::<i32, ()>(&mut left_store, "publish-left")?
        .call(&mut left_store, 0x1122_3344)?;

    let mut right_store = shared_store(&engine, &runtime)?;
    let right_instance = Instance::new(&mut right_store, &module, &[])?;
    right_instance
        .get_typed_func::<i32, ()>(&mut right_store, "publish-right")?
        .call(&mut right_store, 0x5566_7788)?;

    let mut reopened_store = Store::new(&engine, ());
    wasmtime::_internal::transaction_persistence::open_file_backed_storage_for_test(
        &mut reopened_store,
        tmemory_path.clone(),
        tx_log_path.clone(),
    )?;

    let recovered = reopen_and_recover_file_backed_region(&tx_log_path)?;
    assert_eq!(recovered.root_object_ids.len(), 2);
    assert_eq!(
        recovered_graph_leaf_values(&recovered)?,
        vec![0x1122_3344, 0x5566_7788],
    );

    wasmtime::_internal::transaction_persistence::recover_file_backed_tmemory_for_test(
        &tx_log_path,
        &tmemory_path,
        1,
        Some(1),
    )?;
    assert_tmemory_file_bytes(&tmemory_path, 0, &0x1122_3344u32.to_le_bytes())?;
    assert_tmemory_file_bytes(&tmemory_path, 64, &0x5566_7788u32.to_le_bytes())?;

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

fn transaction_persistence_engine() -> Result<Engine> {
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

fn shared_store(
    engine: &Engine,
    runtime: &SharedTransactionRegionRuntimeForTest,
) -> Result<Store<()>> {
    let mut store = Store::new(engine, ());
    adopt_shared_runtime_for_test(&mut store, runtime)?;
    Ok(store)
}

fn warm_shared_module(
    engine: &Engine,
    module: &Module,
    runtime: &SharedTransactionRegionRuntimeForTest,
    noop_imports: usize,
) -> Result<()> {
    let mut store = shared_store(engine, runtime)?;
    let mut imports: Vec<wasmtime::Extern> = Vec::with_capacity(noop_imports);
    for _ in 0..noop_imports {
        imports.push(Func::wrap(&mut store, || {}).into());
    }
    Instance::new(&mut store, module, &imports)?;
    Ok(())
}

fn call_shared_tfunc_i32(
    engine: &Engine,
    module: &Module,
    runtime: &SharedTransactionRegionRuntimeForTest,
    name: &str,
    value: i32,
) -> Result<()> {
    let mut store = shared_store(engine, runtime)?;
    let instance = Instance::new(&mut store, module, &[])?;
    instance
        .get_typed_func::<i32, ()>(&mut store, name)?
        .call(&mut store, value)
}

fn assert_transaction_conflict(error: &str) {
    assert!(
        error.contains("transaction read conflict") || error.contains("transaction write conflict"),
        "{error}"
    );
}

fn threaded_tmemory_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write-left") (param $value i32)
                (i32.tstore (i32.const 0) (local.get $value)))
              (tfunc (export "write-right") (param $value i32)
                (i32.tstore (i32.const 64) (local.get $value))))
            "#,
        )?,
    )
}

fn two_root_object_graph_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (type $leaf (tstruct (field (mut i32))))
              (type $root (tstruct (field (mut (ref null $leaf)))))
              (tglobal $left (mut (ref null $root)) (ref.null $root))
              (tglobal $right (mut (ref null $root)) (ref.null $root))
              (tfunc (export "publish-left") (param $value i32)
                (tglobal.set $left
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value)))))
              (tfunc (export "publish-right") (param $value i32)
                (tglobal.set $right
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value))))))
            "#,
        )?,
    )
}

fn blocking_tmemory_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (import "" "pause" (func $pause))
              (tmemory 1)
              (tfunc (export "write") (param $value i32)
                (i32.tstore (i32.const 0) (local.get $value))
                (drop (tmemory.size))
                (call $pause)))
            "#,
        )?,
    )
}

fn blocking_single_root_object_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (import "" "pause" (func $pause))
              (type $leaf (tstruct (field (mut i32))))
              (type $root (tstruct (field (mut (ref null $leaf)))))
              (tglobal $root (mut (ref null $root)) (ref.null $root))
              (tfunc (export "publish") (param $value i32)
                (tglobal.set $root
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value))))
                (call $pause)))
            "#,
        )?,
    )
}

fn blocking_mixed_tmemory_object_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (import "" "pause" (func $pause))
              (tmemory 1)
              (type $leaf (tstruct (field (mut i32))))
              (type $root (tstruct (field (mut (ref null $leaf)))))
              (tglobal $root (mut (ref null $root)) (ref.null $root))
              (tfunc (export "publish") (param $value i32)
                (i32.tstore (i32.const 0) (local.get $value))
                (drop (tmemory.size))
                (tglobal.set $root
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value))))
                (call $pause)))
            "#,
        )?,
    )
}

fn mixed_tmemory_object_graph_module(engine: &Engine) -> Result<Module> {
    Module::new(
        engine,
        wat::parse_str(
            r#"
            (module
              (tmemory 1)
              (type $leaf (tstruct (field (mut i32))))
              (type $root (tstruct (field (mut (ref null $leaf)))))
              (tglobal $left (mut (ref null $root)) (ref.null $root))
              (tglobal $right (mut (ref null $root)) (ref.null $root))
              (tfunc (export "publish-left") (param $value i32)
                (i32.tstore (i32.const 0) (local.get $value))
                (tglobal.set $left
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value)))))
              (tfunc (export "publish-right") (param $value i32)
                (i32.tstore (i32.const 64) (local.get $value))
                (tglobal.set $right
                  (tstruct.new $root
                    (tstruct.new $leaf (local.get $value))))))
            "#,
        )?,
    )
}

fn recovered_graph_leaf_values(
    recovered: &wasmtime::_internal::transaction_persistence::TransactionPersistenceRecoveredRegion,
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

    let slots = recovered_object_slots(record_bytes, field_count)?;
    let slot = slot_bytes(slots, field_index)?;
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

    let slots = recovered_object_slots(record_bytes, field_count)?;
    let slot = slot_bytes(slots, field_index)?;
    let tag = u32::from_le_bytes(slot[0..4].try_into().unwrap());
    let low = u64::from_le_bytes(slot[4..12].try_into().unwrap());
    let high = u64::from_le_bytes(slot[12..20].try_into().unwrap());
    assert_eq!(tag, OBJECT_VALUE_ABI_TAG_I32);
    assert_eq!(high, 0);
    Ok(low as u32 as i32)
}

fn recovered_object_slots(record_bytes: &[u8], field_count: usize) -> Result<&[u8]> {
    let slots_len = field_count
        .checked_mul(20)
        .expect("field slot byte overflow");
    let field_offset = record_bytes
        .len()
        .checked_sub(slots_len)
        .expect("recovered object record is shorter than its payload");
    Ok(&record_bytes[field_offset..])
}

fn slot_bytes(slots: &[u8], field_index: usize) -> Result<&[u8]> {
    let offset = field_index
        .checked_mul(20)
        .expect("field slot offset overflow");
    let end = offset.checked_add(20).expect("field slot end overflow");
    Ok(slots
        .get(offset..end)
        .expect("missing recovered object field slot"))
}

fn assert_tmemory_file_bytes(tmemory_path: &Path, offset: usize, expected: &[u8]) -> Result<()> {
    let tmemory_bytes = std::fs::read(tmemory_path)?;
    assert_eq!(&tmemory_bytes[offset..offset + expected.len()], expected);
    Ok(())
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
