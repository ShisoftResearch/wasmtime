#![cfg(feature = "transaction")]

use std::mem::MaybeUninit;
use std::process::Command;
use std::ptr;
use wasmtime_c_api::*;

#[test]
fn c_call_rejects_transaction_reference_result_without_aborting() {
    let engine = wasm_engine_new();
    let mut store = wasmtime_store_new(&engine, ptr::null_mut(), None);
    let mut context = wasmtime_store_context(&mut store);
    let module = wasmtime::Module::new(
        context.engine(),
        r#"
            (module
              (type $array (tarray i32))
              (tfunc (export "make") (result (tref $array))
                (tarray.new_default $array (i32.const 1)))
              (func (export "ordinary") (result i32)
                (i32.const 37)))
        "#,
    )
    .unwrap();
    let instance = wasmtime::Instance::new(&mut context, &module, &[]).unwrap();
    let make = instance.get_func(&mut context, "make").unwrap();
    drop(context);

    let mut result = MaybeUninit::<wasmtime_val_t>::uninit();
    let mut trap = ptr::null_mut();
    let error = unsafe {
        wasmtime_func_call(
            wasmtime_store_context(&mut store),
            &make,
            ptr::null(),
            0,
            &mut result,
            1,
            &mut trap,
        )
    };
    assert!(error.is_some());
    assert!(trap.is_null());

    let mut context = wasmtime_store_context(&mut store);
    let ordinary = instance.get_func(&mut context, "ordinary").unwrap();
    drop(context);
    let mut ordinary_result = MaybeUninit::<wasmtime_val_t>::uninit();
    let error = unsafe {
        wasmtime_func_call(
            wasmtime_store_context(&mut store),
            &ordinary,
            ptr::null(),
            0,
            &mut ordinary_result,
            1,
            &mut trap,
        )
    };
    assert!(error.is_none());
    assert!(trap.is_null());
    let ordinary_result = unsafe { ordinary_result.assume_init() };
    assert_eq!(ordinary_result.kind, WASMTIME_I32);
    assert_eq!(unsafe { ordinary_result.of.i32 }, 37);
}

#[cfg(feature = "async")]
#[test]
fn async_c_call_rejects_transaction_reference_result_without_aborting() {
    let engine = wasm_engine_new();
    let mut store = wasmtime_store_new(&engine, ptr::null_mut(), None);
    let mut context = wasmtime_store_context(&mut store);
    let module = wasmtime::Module::new(
        context.engine(),
        r#"
            (module
              (type $array (tarray i32))
              (tfunc (export "make") (result (tref $array))
                (tarray.new_default $array (i32.const 1))))
        "#,
    )
    .unwrap();
    let instance = wasmtime::Instance::new(&mut context, &module, &[]).unwrap();
    let make = instance.get_func(&mut context, "make").unwrap();
    drop(context);

    let mut result = MaybeUninit::<wasmtime_val_t>::uninit();
    let mut trap = ptr::null_mut();
    let mut error = ptr::null_mut();
    let mut future = unsafe {
        wasmtime_func_call_async(
            wasmtime_store_context(&mut store),
            &make,
            ptr::null(),
            0,
            &mut result,
            1,
            &mut trap,
            &mut error,
        )
    };
    assert!(wasmtime_call_future_poll(&mut future));
    drop(future);
    assert!(!error.is_null());
    assert!(trap.is_null());
}

#[cfg(feature = "wat")]
#[test]
fn standard_c_call_rejects_transaction_reference_result_without_aborting() {
    const CHILD: &str = "WASMTIME_TRANSACTION_C_STANDARD_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("standard_c_call_rejects_transaction_reference_result_without_aborting")
            .arg("--nocapture")
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success(), "standard C API child aborted: {status}");
        return;
    }

    let engine = wasm_engine_new();
    let mut store = wasm_store_new(&engine);
    let binary: wasm_byte_vec_t = wat::parse_str(
        r#"
            (module
              (type $array (tarray i32))
              (tfunc (export "make") (result (tref $array))
                (tarray.new_default $array (i32.const 1))))
        "#,
    )
    .unwrap()
    .into();
    let module = unsafe { wasm_module_new(&mut store, &binary) }.unwrap();
    let imports = wasm_extern_vec_t::default();
    let mut instantiate_trap = ptr::null_mut();
    let mut instance =
        unsafe { wasm_instance_new(&mut store, &module, &imports, Some(&mut instantiate_trap)) }
            .unwrap();
    assert!(instantiate_trap.is_null());
    let mut exports = wasm_extern_vec_t::default();
    unsafe { wasm_instance_exports(&mut instance, &mut exports) };
    let mut exports = exports.take();
    let export = exports[0].as_mut().unwrap();
    // `wasm_func_t` is a transparent wrapper around `wasm_extern_t`, and this
    // uniquely borrowed export is known to be a function.
    let func = unsafe { &mut *(export.as_mut() as *mut wasm_extern_t).cast::<wasm_func_t>() };
    let args = wasm_val_vec_t::default();
    let mut results: wasm_val_vec_t = vec![wasm_val_t::default()].into();
    let trap = unsafe { wasm_func_call(func, &args, &mut results) };
    assert!(!trap.is_null());
}
