#![cfg(feature = "transaction")]

use std::mem::MaybeUninit;
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
