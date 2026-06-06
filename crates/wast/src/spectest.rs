use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use wasmtime::*;

/// Configuration of how spectest primitives work.
pub struct SpectestConfig {
    /// Whether or not to have a `shared_memory` definition.
    pub use_shared_memory: bool,
    /// Whether or not spectest functions that print things actually print things.
    pub suppress_prints: bool,
    /// Whether to expose transactional Wasm proposal helper imports.
    pub transaction_helpers: bool,
}

/// Return an instance implementing the "spectest" interface used in the
/// spec testsuite.
pub fn link_spectest<T>(
    linker: &mut Linker<T>,
    store: &mut Store<T>,
    config: &SpectestConfig,
) -> Result<()> {
    let suppress = config.suppress_prints;
    linker.func_wrap("spectest", "print", || {})?;
    linker.func_wrap("spectest", "print_i32", move |val: i32| {
        if !suppress {
            println!("{val}: i32")
        }
    })?;
    linker.func_wrap("spectest", "print_i64", move |val: i64| {
        if !suppress {
            println!("{val}: i64")
        }
    })?;
    linker.func_wrap("spectest", "print_f32", move |val: f32| {
        if !suppress {
            println!("{val}: f32")
        }
    })?;
    linker.func_wrap("spectest", "print_f64", move |val: f64| {
        if !suppress {
            println!("{val}: f64")
        }
    })?;
    linker.func_wrap("spectest", "print_i32_f32", move |i: i32, f: f32| {
        if !suppress {
            println!("{i}: i32");
            println!("{f}: f32");
        }
    })?;
    linker.func_wrap("spectest", "print_f64_f64", move |f1: f64, f2: f64| {
        if !suppress {
            println!("{f1}: f64");
            println!("{f2}: f64");
        }
    })?;
    linker.func_wrap("spectest", "tprint", || {})?;
    linker.func_wrap("spectest", "tprint_i32", move |val: i32| {
        if !suppress {
            println!("{val}: i32")
        }
    })?;
    linker.func_wrap("spectest", "tprint_i64", move |val: i64| {
        if !suppress {
            println!("{val}: i64")
        }
    })?;
    linker.func_wrap("spectest", "tprint_f32", move |val: f32| {
        if !suppress {
            println!("{val}: f32")
        }
    })?;
    linker.func_wrap("spectest", "tprint_f64", move |val: f64| {
        if !suppress {
            println!("{val}: f64")
        }
    })?;
    linker.func_wrap("spectest", "tprint_i32_f32", move |i: i32, f: f32| {
        if !suppress {
            println!("{i}: i32");
            println!("{f}: f32");
        }
    })?;
    linker.func_wrap("spectest", "tprint_f64_f64", move |f1: f64, f2: f64| {
        if !suppress {
            println!("{f1}: f64");
            println!("{f2}: f64");
        }
    })?;

    let ty = GlobalType::new(ValType::I32, Mutability::Const);
    let g = Global::new(&mut *store, ty, Val::I32(666))?;
    linker.define(&mut *store, "spectest", "global_i32", g)?;
    linker.define(&mut *store, "spectest", "tglobal_i32", g)?;

    let ty = GlobalType::new(ValType::I64, Mutability::Const);
    let g = Global::new(&mut *store, ty, Val::I64(666))?;
    linker.define(&mut *store, "spectest", "global_i64", g)?;
    linker.define(&mut *store, "spectest", "tglobal_i64", g)?;

    let ty = GlobalType::new(ValType::F32, Mutability::Const);
    let g = Global::new(&mut *store, ty, Val::F32(0x4426_a666))?;
    linker.define(&mut *store, "spectest", "global_f32", g)?;
    linker.define(&mut *store, "spectest", "tglobal_f32", g)?;

    let ty = GlobalType::new(ValType::F64, Mutability::Const);
    let g = Global::new(&mut *store, ty, Val::F64(0x4084_d4cc_cccc_cccd))?;
    linker.define(&mut *store, "spectest", "global_f64", g)?;
    linker.define(&mut *store, "spectest", "tglobal_f64", g)?;

    let ty = TableType::new(RefType::FUNCREF, 10, Some(20));
    let table = Table::new(&mut *store, ty, Ref::Func(None))?;
    linker.define(&mut *store, "spectest", "table", table)?;
    linker.define(&mut *store, "spectest", "ttable", table)?;

    let ty = TableType::new64(RefType::FUNCREF, 10, Some(20));
    let table = Table::new(&mut *store, ty, Ref::Func(None))?;
    linker.define(&mut *store, "spectest", "table64", table)?;

    let ty = MemoryType::new(1, Some(2));
    let memory = Memory::new(&mut *store, ty)?;
    linker.define(&mut *store, "spectest", "memory", memory)?;

    let transaction_memory = Module::new(
        store.engine(),
        r#"(module (tmemory (export "tmemory") 1 2))"#,
    )?;
    let transaction_memory = Instance::new(&mut *store, &transaction_memory, &[])?;
    let transaction_memory = transaction_memory
        .get_memory(&mut *store, "tmemory")
        .expect("transaction memory module exports tmemory");
    linker.define(&mut *store, "spectest", "tmemory", transaction_memory)?;

    if config.transaction_helpers {
        link_transaction_spectest_helpers(linker, store)?;
    }

    if config.use_shared_memory {
        let ty = MemoryType::shared(1, 1);
        let memory = SharedMemory::new(store.engine(), ty)?;
        linker.define(&mut *store, "spectest", "shared_memory", memory)?;
    }

    Ok(())
}

#[derive(Default)]
struct TransactionSpectestState {
    active: BTreeSet<i32>,
    next_lo_tid: i32,
    next_hi_tid: i32,
    next_current_tid: i32,
}

impl TransactionSpectestState {
    fn current_tid(&self) -> i32 {
        0
    }

    fn next_tcurrent_tid(&mut self) -> i32 {
        self.next_current_tid += 1;
        10_000 + self.next_current_tid
    }

    fn next_lo_tid(&mut self) -> i32 {
        self.next_lo_tid += 1;
        let tid = self.next_lo_tid;
        self.active.insert(tid);
        tid
    }

    fn next_hi_tid(&mut self) -> i32 {
        self.next_hi_tid += 1;
        let tid = 100_000 + self.next_hi_tid;
        self.active.insert(tid);
        tid
    }

    fn activate(&mut self, tid: i32) {
        if tid != 0 {
            self.active.insert(tid);
        }
    }

    fn finish(&mut self, tid: i32) -> i32 {
        if self.active.remove(&tid) { 0 } else { 2 }
    }
}

fn with_transaction_spectest_state(
    state: &Arc<Mutex<TransactionSpectestState>>,
    f: impl FnOnce(&mut TransactionSpectestState) -> i32,
) -> Result<i32> {
    let mut state = state
        .lock()
        .map_err(|_| Error::msg("transaction spectest state lock poisoned"))?;
    Ok(f(&mut state))
}

// SHISOFT-TWASM-MOCK: proposal spectest transaction helper imports.
// This is a deterministic harness scaffold, not the real Wizard scheduler. It
// lets proposal WAST instantiate while real transaction concurrency control and
// object-table ownership move into runtime paths.
fn link_transaction_spectest_helpers<T>(linker: &mut Linker<T>, store: &mut Store<T>) -> Result<()>
where
    T: 'static,
{
    let state = Arc::new(Mutex::new(TransactionSpectestState::default()));

    linker.func_wrap("spectest", "current_tid", {
        let state = state.clone();
        move || with_transaction_spectest_state(&state, |state| state.current_tid())
    })?;
    linker.func_wrap("spectest", "tcurrent_tid", {
        let state = state.clone();
        move || with_transaction_spectest_state(&state, |state| state.next_tcurrent_tid())
    })?;

    for name in ["next_lo_tid", "tnext_lo_tid"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move || with_transaction_spectest_state(&state, |state| state.next_lo_tid())
        })?;
    }
    for name in ["next_hi_tid", "tnext_hi_tid"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move || with_transaction_spectest_state(&state, |state| state.next_hi_tid())
        })?;
    }

    for name in ["ttable_granule_size", "tttable_granule_size"] {
        linker.func_wrap("spectest", name, || -> i32 { 16 })?;
    }
    for name in ["tmemory_granule_size", "ttmemory_granule_size"] {
        linker.func_wrap("spectest", name, || -> i32 { 256 })?;
    }

    for name in ["abort_txn", "tabort_txn", "tcommit_txn"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move |tid: i32| with_transaction_spectest_state(&state, |state| state.finish(tid))
        })?;
    }

    let run_as_ty = FuncType::new(
        store.engine(),
        [ValType::I32, ValType::FUNCREF, ValType::ANYREF],
        [ValType::I32, ValType::ANYREF],
    );
    for name in ["run_as_tid", "trun_as_tid"] {
        linker.func_new("spectest", name, run_as_ty.clone(), {
            let state = state.clone();
            move |mut caller, params, results| {
                let tid = match params {
                    [Val::I32(tid), Val::FuncRef(_), Val::AnyRef(_)] => *tid,
                    _ => {
                        results[0] = Val::I32(3);
                        results[1] = Val::AnyRef(None);
                        return Ok(());
                    }
                };
                let Some(Some(func)) = params[1].funcref() else {
                    results[0] = Val::I32(3);
                    results[1] = Val::AnyRef(None);
                    return Ok(());
                };
                with_transaction_spectest_state(&state, |state| {
                    state.activate(tid);
                    0
                })?;

                let mut call_results = [Val::AnyRef(None)];
                func.call(&mut caller, &params[2..3], &mut call_results)?;
                results[0] = Val::I32(0);
                results[1] = call_results[0].clone();
                Ok(())
            }
        })?;
    }

    Ok(())
}

#[cfg(feature = "component-model")]
pub fn link_component_spectest<T>(linker: &mut component::Linker<T>) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    use wasmtime::component::{Resource, ResourceType};

    let engine = linker.engine().clone();
    linker
        .root()
        .func_wrap_concurrent("host-echo-u32", |_, (v,): (u32,)| {
            Box::pin(async move { Ok((v,)) })
        })?;
    linker
        .root()
        .func_wrap("host-return-two", |_, _: ()| Ok((2u32,)))?;
    let mut i = linker.instance("host")?;
    i.func_wrap("return-three", |_, _: ()| Ok((3u32,)))?;
    i.instance("nested")?
        .func_wrap("return-four", |_, _: ()| Ok((4u32,)))?;

    if !cfg!(miri) {
        let module = Module::new(
            &engine,
            r#"
                (module
                    (global (export "g") i32 i32.const 100)
                    (func (export "f") (result i32) i32.const 101)
                )
            "#,
        )?;
        i.module("simple-module", &module)?;
    }

    struct Resource1;
    struct Resource2;

    #[derive(Default)]
    struct ResourceState {
        drops: AtomicU32,
        last_drop: AtomicU32,
    }

    let state = Arc::new(ResourceState::default());

    i.resource("resource1", ResourceType::host::<Resource1>(), {
        let state = state.clone();
        move |_, rep| {
            state.drops.fetch_add(1, SeqCst);
            state.last_drop.store(rep, SeqCst);

            Ok(())
        }
    })?;
    i.resource(
        "resource2",
        ResourceType::host::<Resource2>(),
        |_, _| Ok(()),
    )?;
    // Currently the embedder API requires redefining the resource destructor
    // here despite this being the same type as before, and fixing that is left
    // for a future refactoring.
    i.resource(
        "resource1-again",
        ResourceType::host::<Resource1>(),
        |_, _| {
            panic!("shouldn't be destroyed");
        },
    )?;

    i.func_wrap("[constructor]resource1", |_cx, (rep,): (u32,)| {
        Ok((Resource::<Resource1>::new_own(rep),))
    })?;
    i.func_wrap(
        "[static]resource1.assert",
        |_cx, (resource, rep): (Resource<Resource1>, u32)| {
            assert_eq!(resource.rep(), rep);
            Ok(())
        },
    )?;
    i.func_wrap("[static]resource1.last-drop", {
        let state = state.clone();
        move |_, (): ()| Ok((state.last_drop.load(SeqCst),))
    })?;
    i.func_wrap("[static]resource1.drops", {
        let state = state.clone();
        move |_, (): ()| Ok((state.drops.load(SeqCst),))
    })?;
    i.func_wrap(
        "[method]resource1.simple",
        |_cx, (resource, rep): (Resource<Resource1>, u32)| {
            assert!(!resource.owned());
            assert_eq!(resource.rep(), rep);
            Ok(())
        },
    )?;

    i.func_wrap(
        "[method]resource1.take-borrow",
        |_, (a, b): (Resource<Resource1>, Resource<Resource1>)| {
            assert!(!a.owned());
            assert!(!b.owned());
            Ok(())
        },
    )?;
    i.func_wrap(
        "[method]resource1.take-own",
        |_cx, (a, b): (Resource<Resource1>, Resource<Resource1>)| {
            assert!(!a.owned());
            assert!(b.owned());
            Ok(())
        },
    )?;
    i.func_wrap_concurrent("never-return", |_, _: ()| {
        Box::pin(async move { std::future::pending::<Result<()>>().await })
    })?;
    i.func_wrap_concurrent("return-two-slowly", |_, _: ()| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok((2,))
        })
    })?;
    i.func_wrap_concurrent("echo-slowly", |_, (a,): (u32,)| {
        Box::pin(async move {
            tokio::task::yield_now().await;
            Ok((a,))
        })
    })?;
    i.func_wrap_concurrent(
        "[method]resource1.never-return",
        |_, (_,): (Resource<Resource1>,)| {
            Box::pin(async move { std::future::pending::<Result<()>>().await })
        },
    )?;
    i.func_wrap("return-hi", |_cx, (): ()| Ok(("hi".to_string(),)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_spectest_helpers_are_opt_in() -> Result<()> {
        let mut config = Config::new();
        config.wasm_reference_types(false);
        config.wasm_function_references(false);
        config.wasm_gc(false);
        config.wasm_features(WasmFeatures::GC_TYPES, false);
        let engine = Engine::new(&config)?;

        let module = Module::new(
            &engine,
            r#"
                (module
                    (import "spectest" "current_tid" (func $current_tid (result i32)))
                    (func (export "current") (result i32) (call $current_tid))
                )
            "#,
        )?;

        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        link_spectest(
            &mut linker,
            &mut store,
            &SpectestConfig {
                use_shared_memory: false,
                suppress_prints: true,
                transaction_helpers: false,
            },
        )?;
        assert!(linker.instantiate(&mut store, &module).is_err());

        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        link_spectest(
            &mut linker,
            &mut store,
            &SpectestConfig {
                use_shared_memory: false,
                suppress_prints: true,
                transaction_helpers: true,
            },
        )?;
        let instance = linker.instantiate(&mut store, &module)?;
        let current = instance.get_typed_func::<(), i32>(&mut store, "current")?;
        assert_eq!(current.call(&mut store, ())?, 0);

        Ok(())
    }

    #[test]
    fn transaction_spectest_helpers_match_fixture_ids_and_granules() -> Result<()> {
        let mut config = Config::new();
        config.wasm_reference_types(false);
        config.wasm_function_references(false);
        config.wasm_gc(false);
        config.wasm_features(WasmFeatures::GC_TYPES, false);
        let engine = Engine::new(&config)?;
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        link_spectest(
            &mut linker,
            &mut store,
            &SpectestConfig {
                use_shared_memory: false,
                suppress_prints: true,
                transaction_helpers: true,
            },
        )?;

        let module = Module::new(
            &engine,
            r#"
                (module
                    (import "spectest" "tcurrent_tid" (func $tcurrent_tid (result i32)))
                    (import "spectest" "next_lo_tid" (func $next_lo_tid (result i32)))
                    (import "spectest" "tnext_lo_tid" (func $tnext_lo_tid (result i32)))
                    (import "spectest" "next_hi_tid" (func $next_hi_tid (result i32)))
                    (import "spectest" "tnext_hi_tid" (func $tnext_hi_tid (result i32)))
                    (import "spectest" "tmemory_granule_size" (func $tmemory_granule_size (result i32)))
                    (import "spectest" "ttable_granule_size" (func $ttable_granule_size (result i32)))
                    (import "spectest" "abort_txn" (func $abort_txn (param i32) (result i32)))

                    (func (export "tcurrent") (result i32) (call $tcurrent_tid))
                    (func (export "lo") (result i32) (call $next_lo_tid))
                    (func (export "tlo") (result i32) (call $tnext_lo_tid))
                    (func (export "hi") (result i32) (call $next_hi_tid))
                    (func (export "thi") (result i32) (call $tnext_hi_tid))
                    (func (export "tmemory_granule") (result i32) (call $tmemory_granule_size))
                    (func (export "ttable_granule") (result i32) (call $ttable_granule_size))
                    (func (export "abort") (param i32) (result i32)
                        (call $abort_txn (local.get 0)))
                )
            "#,
        )?;
        let instance = linker.instantiate(&mut store, &module)?;

        let call = |store: &mut Store<()>, name| -> Result<i32> {
            instance
                .get_typed_func::<(), i32>(&mut *store, name)?
                .call(&mut *store, ())
        };
        let abort = instance.get_typed_func::<i32, i32>(&mut store, "abort")?;

        assert_eq!(call(&mut store, "tcurrent")?, 10_001);
        assert_eq!(call(&mut store, "tcurrent")?, 10_002);
        assert_eq!(call(&mut store, "lo")?, 1);
        assert_eq!(abort.call(&mut store, 1)?, 0);
        assert_eq!(call(&mut store, "tlo")?, 2);
        assert_eq!(abort.call(&mut store, 2)?, 0);
        assert_eq!(call(&mut store, "hi")?, 100_001);
        assert_eq!(abort.call(&mut store, 100_001)?, 0);
        assert_eq!(call(&mut store, "thi")?, 100_002);
        assert_eq!(abort.call(&mut store, 100_002)?, 0);
        assert_eq!(call(&mut store, "tmemory_granule")?, 256);
        assert_eq!(call(&mut store, "ttable_granule")?, 16);

        Ok(())
    }
}
