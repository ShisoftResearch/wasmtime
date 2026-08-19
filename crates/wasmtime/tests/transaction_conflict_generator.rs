use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use wasmtime::{
    Caller, Engine, FuncType, Instance, Linker, Module, Result, Store, TransactionModuleNamespace,
    V128, Val, ValType,
};

#[derive(Clone, Copy)]
struct GeneratedType {
    wasm_type: &'static str,
    zero_constructor: &'static str,
}

const REFERENCE_TYPE_MATRIX: [GeneratedType; 3] = [
    GeneratedType {
        wasm_type: "i32",
        zero_constructor: "i32.const",
    },
    GeneratedType {
        wasm_type: "i64",
        zero_constructor: "i64.const",
    },
    GeneratedType {
        wasm_type: "v128",
        zero_constructor: "v128.const i64x2 0",
    },
];

// Semantic Rust port of `tconflict-tmemory.py`'s generated module. The source
// script is not independently valid: its obsolete `(tblock (return ...))`
// spelling and `$str2`/`$str3` result casts disagree with the declared types.
// Those operations are expressed below with the proposal-native typed-block,
// table/element, and cast forms while preserving the generated schedule.
const MODULE_TEMPLATE: &str = r#"
(module
  (type $str1 (tstruct
    (field i32)
    (field <TYPE1>)))
  (type $str2 (tstruct
    (field i32)
    (field <TYPE2>)))
  (type $str3 (tstruct
    (field i32)
    (field <TYPE3>)))
  (type $ft (tfunc (param tanyref) (result tanyref)))

  (func $tmemory_granule_size
    (import "spectest" "tmemory_granule_size") (result i32))
  (tfunc $ttmemory_granule_size
    (import "spectest" "ttmemory_granule_size") (result i32))
  (func $run_as
    (import "spectest" "run_as_tid")
    (param $tid i32) (param $func tfuncref) (param $arg tanyref)
    (result i32 tanyref))
  (tfunc $trun_as
    (import "spectest" "trun_as_tid")
    (param $tid i32) (param $func tfuncref) (param $arg tanyref)
    (result i32 tanyref))
  (func $abort_txn
    (import "spectest" "abort_txn") (param $tid i32) (result i32))
  (tfunc $tabort_txn
    (import "spectest" "tabort_txn") (param $tid i32) (result i32))
  (tfunc $tcommit_txn
    (import "spectest" "tcommit_txn") (param $tid i32) (result i32))
  (tfunc $tnext_lo_tid
    (import "spectest" "tnext_lo_tid") (result i32))
  (tfunc $tnext_hi_tid
    (import "spectest" "tnext_hi_tid") (result i32))

  (tmemory 1)

  (tfunc $t1_get_func (param $arg tanyref) (result tanyref)
    (local $s (tref read $str1))
    (local $v <TYPE1>)
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str1))
      (br_on_tcast $l1 tanyref (tref $str1))
      (drop)
      (tstruct.new $str1 (i32.const 0) (<CONST1> 0)))
    (tref.cast_read)
    (local.set $s)
    (<TYPE1>.tload (tstruct.get $str1 0 (local.get $s)))
    (local.set $v)
    (tstruct.new $str1 (i32.const 0) (local.get $v)))

  (tfunc $t1_set_func (param $arg tanyref) (result tanyref)
    (local $s (tref write $str1))
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str1))
      (br_on_tcast $l1 tanyref (tref $str1))
      (drop)
      (tstruct.new $str1 (i32.const 0) (<CONST1> 0)))
    (tref.cast_write)
    (local.set $s)
    (<TYPE1>.tstore
      (tstruct.get $str1 0 (local.get $s))
      (tstruct.get $str1 1 (local.get $s)))
    (local.get $s))

  (tfunc $t2_get_func (param $arg tanyref) (result tanyref)
    (local $s (tref read $str2))
    (local $v <TYPE2>)
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str2))
      (br_on_tcast $l1 tanyref (tref $str2))
      (drop)
      (tstruct.new $str2 (i32.const 0) (<CONST2> 0)))
    (tref.cast_read)
    (local.set $s)
    (<TYPE2>.tload (tstruct.get $str2 0 (local.get $s)))
    (local.set $v)
    (tstruct.new $str2 (i32.const 0) (local.get $v)))

  (tfunc $t2_set_func (param $arg tanyref) (result tanyref)
    (local $s (tref write $str2))
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str2))
      (br_on_tcast $l1 tanyref (tref $str2))
      (drop)
      (tstruct.new $str2 (i32.const 0) (<CONST2> 0)))
    (tref.cast_write)
    (local.set $s)
    (<TYPE2>.tstore
      (tstruct.get $str2 0 (local.get $s))
      (tstruct.get $str2 1 (local.get $s)))
    (local.get $s))

  (tfunc $t3_get_func (param $arg tanyref) (result tanyref)
    (local $s (tref read $str3))
    (local $v <TYPE3>)
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str3))
      (br_on_tcast $l1 tanyref (tref $str3))
      (drop)
      (tstruct.new $str3 (i32.const 0) (<CONST3> 0)))
    (tref.cast_read)
    (local.set $s)
    (<TYPE3>.tload (tstruct.get $str3 0 (local.get $s)))
    (local.set $v)
    (tstruct.new $str3 (i32.const 0) (local.get $v)))

  (tfunc $t3_set_func (param $arg tanyref) (result tanyref)
    (local $s (tref write $str3))
    (local.get $arg)
    (block $l1 (param tanyref) (result (tref $str3))
      (br_on_tcast $l1 tanyref (tref $str3))
      (drop)
      (tstruct.new $str3 (i32.const 0) (<CONST3> 0)))
    (tref.cast_write)
    (local.set $s)
    (<TYPE3>.tstore
      (tstruct.get $str3 0 (local.get $s))
      (tstruct.get $str3 1 (local.get $s)))
    (local.get $s))

  (tfunc $t1_make_func
    (param $fst i32) (param $snd <TYPE1>) (result (tref write $str1))
    (tstruct.new $str1 (local.get $fst) (local.get $snd)))

  (tfunc $tdo_run_as_tid (export "tdo_run_as_tid")
    (param $tid i32) (param $func tfuncref) (param $arg tanyref)
    (result i32 tanyref)
    (tcall $trun_as (local.get $tid) (local.get $func) (local.get $arg)))

  (ttable $ttab0 6 tfuncref)
  (telem (ttable $ttab0) (i32.const 0) tfuncref
    (item tref.tfunc $t1_get_func)
    (item tref.tfunc $t1_set_func)
    (item tref.tfunc $t2_get_func)
    (item tref.tfunc $t2_set_func)
    (item tref.tfunc $t3_get_func)
    (item tref.tfunc $t3_set_func))

  (func $do_general_conflict_test (export "general_conflict_test")
    (param $fst_lo i32)
    (param $fst_fidx i32) (param $fst_addr i32) (param $fst_value <TYPE1>)
    (param $snd_fidx i32) (param $snd_addr i32) (param $snd_value <TYPE2>)
    (param $commit_snd i32)
    (param $lst_fidx i32) (param $lst_addr i32) (param $lst_value <TYPE3>)
    (result <TYPE1> i32 i32 <TYPE2> i32 i32 <TYPE3> i32)

    (local $ftid i32)
    (local $fretval <TYPE1>)
    (local $fcode1 i32)
    (local $fcode2 i32)
    (local $stid i32)
    (local $sretval <TYPE2>)
    (local $scode1 i32)
    (local $scode2 i32)
    (local $lretval <TYPE3>)
    (local $lcode i32)

    (if (result i32 i32)
      (local.get $fst_lo)
      (then
        (tcall $tnext_lo_tid)
        (tcall $tnext_hi_tid))
      (else
        (tcall $tnext_hi_tid)
        (tcall $tnext_lo_tid)))
    (local.set $stid)
    (local.set $ftid)

    (tblock (result i32 <TYPE1>)
      ((tcall $tdo_run_as_tid
          (local.get $ftid)
          (ttable.get (local.get $fst_fidx))
          (tstruct.new $str1 (local.get $fst_addr) (local.get $fst_value)))
       (block $l (param tanyref) (result (tref $str1))
         (br_on_tcast $l tanyref (tref $str1))
         (drop)
         (tstruct.new $str1 (i32.const 0) (<CONST1> 0)))
       (tref.cast_read)
       (tstruct.get $str1 1))
      (else
        (drop)
        (i32.const 9)
        (<CONST1> 0)))
    (local.set $fretval)
    (local.set $fcode1)

    (local.set $sretval (<CONST2> -1))
    (local.set $scode1 (i32.const 0))
    (tblock
      ((tcall $tdo_run_as_tid
          (local.get $stid)
          (ttable.get (local.get $snd_fidx))
          (tstruct.new $str2 (local.get $snd_addr) (local.get $snd_value)))
       (block $l (param tanyref) (result (tref $str2))
         (br_on_tcast $l tanyref (tref $str2))
         (drop)
         (tstruct.new $str2 (i32.const 0) (<CONST2> -1)))
       (tref.cast_read)
       (tstruct.get $str2 1)
       (local.set $sretval)
       (local.set $scode1))
      (else
        (drop)
        (local.set $scode1 (i32.const 9))))

    (tcall $tcommit_txn (local.get $ftid))
    (local.set $fcode2)
    (if (result i32)
      (local.get $commit_snd)
      (then (tcall $tcommit_txn (local.get $stid)))
      (else (tcall $tabort_txn (local.get $stid))))
    (local.set $scode2)

    (tblock
      ((tcall_ref $ft
          (tstruct.new $str3 (local.get $lst_addr) (local.get $lst_value))
          (tref.cast (tref $ft) (ttable.get (local.get $lst_fidx))))
       (block $l (param tanyref) (result (tref $str3))
         (br_on_tcast $l tanyref (tref $str3))
         (drop)
         (tstruct.new $str3 (i32.const 0) (<CONST3> -1)))
       (tref.cast_read)
       (tstruct.get $str3 1)
       (local.set $lretval)
       (local.set $lcode (i32.const 0)))
      (else
        (drop)
        (local.set $lretval (<CONST3> -1))
        (local.set $lcode (i32.const 9))))

    (local.get $fretval)
    (local.get $fcode1)
    (local.get $fcode2)
    (local.get $sretval)
    (local.get $scode1)
    (local.get $scode2)
    (local.get $lretval)
    (local.get $lcode)))
"#;

fn generated_tmemory_module() -> String {
    let [first, second, last] = REFERENCE_TYPE_MATRIX;
    MODULE_TEMPLATE
        .replace("<TYPE1>", first.wasm_type)
        .replace("<TYPE2>", second.wasm_type)
        .replace("<TYPE3>", last.wasm_type)
        .replace("<CONST1>", first.zero_constructor)
        .replace("<CONST2>", second.zero_constructor)
        .replace("<CONST3>", last.zero_constructor)
}

#[derive(Default)]
struct TransactionScheduleState {
    active: BTreeSet<i32>,
    entered: BTreeSet<i32>,
    next_lo_tid: i32,
    next_hi_tid: i32,
}

impl TransactionScheduleState {
    fn next_lo_tid(&mut self) -> i32 {
        self.next_lo_tid += 1;
        self.active.insert(self.next_lo_tid);
        self.next_lo_tid
    }

    fn next_hi_tid(&mut self) -> i32 {
        self.next_hi_tid += 1;
        let tid = 100_000 + self.next_hi_tid;
        self.active.insert(tid);
        tid
    }

    fn activate(&mut self, tid: i32) {
        self.active.insert(tid);
        self.entered.insert(tid);
    }

    fn finish(&mut self, tid: i32) -> i32 {
        self.entered.remove(&tid);
        if self.active.remove(&tid) { 0 } else { 2 }
    }

    fn finish_missing_runtime_transaction(&mut self, tid: i32) -> i32 {
        if self.entered.contains(&tid) {
            self.finish(tid);
            2
        } else {
            self.finish(tid)
        }
    }
}

fn with_schedule_state(
    state: &Arc<Mutex<TransactionScheduleState>>,
    f: impl FnOnce(&mut TransactionScheduleState) -> i32,
) -> Result<i32> {
    let mut state = state
        .lock()
        .map_err(|_| wasmtime::Error::msg("transaction schedule state lock poisoned"))?;
    Ok(f(&mut state))
}

fn runtime_tid(tid: i32) -> Option<u64> {
    u64::try_from(tid).ok().filter(|tid| *tid != 0)
}

fn link_reference_schedule(linker: &mut Linker<()>, store: &mut Store<()>) -> Result<()> {
    let state = Arc::new(Mutex::new(TransactionScheduleState::default()));

    for name in ["tnext_lo_tid"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move || with_schedule_state(&state, |state| state.next_lo_tid())
        })?;
    }
    for name in ["tnext_hi_tid"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move || with_schedule_state(&state, |state| state.next_hi_tid())
        })?;
    }
    for name in ["tmemory_granule_size", "ttmemory_granule_size"] {
        linker.func_wrap("spectest", name, || -> i32 { 64 })?;
    }

    for name in ["abort_txn", "tabort_txn"] {
        linker.func_wrap("spectest", name, {
            let state = state.clone();
            move |mut caller: Caller<'_, ()>, tid: i32| -> Result<i32> {
                let Some(runtime_tid) = runtime_tid(tid) else {
                    return Ok(2);
                };
                let runtime_aborted = caller.transaction_spectest_abort_tid(runtime_tid)?;
                with_schedule_state(&state, |state| {
                    if runtime_aborted {
                        state.finish(tid);
                        0
                    } else {
                        state.finish_missing_runtime_transaction(tid)
                    }
                })
            }
        })?;
    }
    linker.func_wrap("spectest", "tcommit_txn", {
        let state = state.clone();
        move |mut caller: Caller<'_, ()>, tid: i32| -> Result<i32> {
            let Some(runtime_tid) = runtime_tid(tid) else {
                return Ok(2);
            };
            match caller.transaction_spectest_commit_tid(runtime_tid) {
                Ok(true) => {
                    with_schedule_state(&state, |state| state.finish(tid))?;
                    Ok(0)
                }
                Ok(false) => with_schedule_state(&state, |state| {
                    state.finish_missing_runtime_transaction(tid)
                }),
                Err(_) => {
                    let _ = caller.transaction_spectest_abort_tid(runtime_tid);
                    with_schedule_state(&state, |state| state.finish(tid))?;
                    Ok(1)
                }
            }
        }
    })?;

    let tanyref = wasmtime::_internal::transaction_persistence::transaction_wast_tanyref_type();
    let run_as_type = FuncType::new(
        store.engine(),
        [
            ValType::I32,
            wasmtime::_internal::transaction_persistence::transaction_wast_tfuncref_type(),
            tanyref.clone(),
        ],
        [ValType::I32, tanyref],
    );
    for name in ["run_as_tid", "trun_as_tid"] {
        linker.func_new("spectest", name, run_as_type.clone(), {
            let state = state.clone();
            move |mut caller, params, results| {
                let tid = match params {
                    [
                        Val::I32(tid),
                        Val::TransactionFuncRef(_),
                        Val::TransactionRef(_) | Val::TransactionExternRef(_) | Val::I32(_),
                    ] => *tid,
                    _ => {
                        results[0] = Val::I32(3);
                        results[1] = wasmtime::_internal::transaction_persistence::transaction_wast_tany_null();
                        return Ok(());
                    }
                };
                let Val::TransactionFuncRef(Some(func)) = &params[1] else {
                    results[0] = Val::I32(3);
                    results[1] =
                        wasmtime::_internal::transaction_persistence::transaction_wast_tany_null();
                    return Ok(());
                };
                let Some(runtime_tid) = runtime_tid(tid) else {
                    results[0] = Val::I32(2);
                    results[1] =
                        wasmtime::_internal::transaction_persistence::transaction_wast_tany_null();
                    return Ok(());
                };
                let previous = match caller.transaction_spectest_enter_tid(runtime_tid) {
                    Ok(previous) => previous,
                    Err(error) if error.to_string().contains("already current") => {
                        results[0] = Val::I32(2);
                        results[1] =
                            wasmtime::_internal::transaction_persistence::transaction_wast_tany_null();
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                with_schedule_state(&state, |state| {
                    state.activate(tid);
                    0
                })?;

                let mut call_results = [
                    wasmtime::_internal::transaction_persistence::transaction_wast_tany_null(),
                ];
                let call_result = func.call(&mut caller, &params[2..3], &mut call_results);
                caller.transaction_spectest_restore_tid(previous)?;
                match call_result {
                    Ok(()) => {
                        results[0] = Val::I32(0);
                        results[1] = call_results[0].clone();
                    }
                    Err(_) => {
                        let _ = caller.transaction_spectest_abort_tid(runtime_tid);
                        with_schedule_state(&state, |state| state.finish(tid))?;
                        results[0] = Val::I32(1);
                        results[1] =
                            wasmtime::_internal::transaction_persistence::transaction_wast_tany_null();
                    }
                }
                Ok(())
            }
        })?;
    }
    Ok(())
}

fn instantiate_generated_module(engine: &Engine) -> Result<(Store<()>, Instance)> {
    let wasm = wat::parse_str(generated_tmemory_module())?;
    let module = Module::new(engine, &wasm)?;
    let mut store = Store::new(engine, ());
    let mut linker = Linker::new(engine);
    link_reference_schedule(&mut linker, &mut store)?;
    let instance = linker.instantiate_with_transaction_module_namespace(
        &mut store,
        &module,
        TransactionModuleNamespace::new(0x7463_6f6e_666c_6963),
    )?;
    Ok((store, instance))
}

#[test]
fn generated_module_ports_reference_scheduler_and_type_matrix() {
    let module = generated_tmemory_module();
    for required in [
        "(type $str1 (tstruct",
        "(type $str2 (tstruct",
        "(type $str3 (tstruct",
        "(func $run_as",
        "(tfunc $trun_as",
        "(ttable $ttab0 6 tfuncref)",
        "(func $do_general_conflict_test (export \"general_conflict_test\")",
    ] {
        assert!(module.contains(required), "missing `{required}`");
    }
    assert!(!module.contains("<TYPE"));
    assert!(!module.contains("<CONST"));
}

#[test]
fn generated_tmemory_demo_matches_reference_observable_eight_result_tuple() -> Result<()> {
    let engine = Engine::default();
    let (mut store, instance) = instantiate_generated_module(&engine)?;
    let test = instance
        .get_func(&mut store, "general_conflict_test")
        .expect("generated module exports general_conflict_test");

    // Exact `gen_demo_test` schedule: t1_set(0, 1), t2_get(128, 0), commit
    // the second transaction, then t3_get(0, v128.zero). The Python file's
    // stale assertion says the outer transaction can read the write-owned
    // aggregate returned by t1_set. Reference ownership rules instead abort
    // that outer read, while tid1 remains independently committable. The last
    // v128 load observes the committed i32 store's low bit.
    let params = [
        Val::I32(1),
        Val::I32(1),
        Val::I32(0),
        Val::I32(1),
        Val::I32(2),
        Val::I32(128),
        Val::I64(0),
        Val::I32(1),
        Val::I32(4),
        Val::I32(0),
        Val::V128(V128::from(0)),
    ];
    let mut results = [
        Val::I32(-1),
        Val::I32(-1),
        Val::I32(-1),
        Val::I64(-1),
        Val::I32(-1),
        Val::I32(-1),
        Val::V128(V128::from(u128::MAX)),
        Val::I32(-1),
    ];
    test.call(&mut store, &params, &mut results)?;

    assert!(matches!(results[0], Val::I32(0)));
    assert!(matches!(results[1], Val::I32(9)));
    assert!(matches!(results[2], Val::I32(0)));
    assert!(matches!(results[3], Val::I64(0)));
    assert!(matches!(results[4], Val::I32(0)));
    assert!(matches!(results[5], Val::I32(0)));
    assert!(matches!(results[6], Val::V128(value) if u128::from(value) == 1));
    assert!(matches!(results[7], Val::I32(0)));
    Ok(())
}
