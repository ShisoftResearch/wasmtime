use wasmtime_wast::{Async, WastContext};

#[test]
fn invoke_rejects_excess_arguments_before_zipping_types() {
    let mut config = wasmtime::Config::new();
    config.wasm_gc(false);
    let engine = wasmtime::Engine::new(&config).unwrap();
    let mut context = WastContext::new(&engine, Async::No, |_| {});
    let error = context
        .run_wast(
            "arity.wast",
            br#"
                (module (func (export "take") (param i32)))
                (invoke "take" (i32.const 1) (i32.const 2))
            "#,
        )
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("expected 1 arguments, found 2"),
        "{message}"
    );
}
