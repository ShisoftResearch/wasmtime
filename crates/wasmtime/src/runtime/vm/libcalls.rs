//! Runtime library calls.
//!
//! Note that Wasm compilers may sometimes perform these inline rather than
//! calling them, particularly when CPUs have special instructions which compute
//! them directly.
//!
//! These functions are called by compiled Wasm code, and therefore must take
//! certain care about some things:
//!
//! * They must only contain basic, raw i32/i64/f32/f64/pointer parameters that
//!   are safe to pass across the system ABI.
//!
//! * If any nested function propagates an `Err(trap)` out to the library
//!   function frame, we need to raise it. This involves some nasty and quite
//!   unsafe code under the covers! Notably, after raising the trap, drops
//!   **will not** be run for local variables! This can lead to things like
//!   leaking `InstanceHandle`s which leads to never deallocating JIT code,
//!   instances, and modules if we are not careful!
//!
//! * The libcall must be entered via a Wasm-to-libcall trampoline that saves
//!   the last Wasm FP and PC for stack walking purposes. (For more details, see
//!   `crates/wasmtime/src/runtime/vm/backtrace.rs`.)
//!
//! To make it easier to correctly handle all these things, **all** libcalls
//! must be defined via the `libcall!` helper macro! See its doc comments below
//! for an example, or just look at the rest of the file.
//!
//! ## Dealing with `externref`s
//!
//! When receiving a raw `*mut u8` that is actually a `VMExternRef` reference,
//! convert it into a proper `VMExternRef` with `VMExternRef::clone_from_raw` as
//! soon as apossible. Any GC before raw pointer is converted into a reference
//! can potentially collect the referenced object, which could lead to use after
//! free.
//!
//! Avoid this by eagerly converting into a proper `VMExternRef`! (Unfortunately
//! there is no macro to help us automatically get this correct, so stay
//! vigilant!)
//!
//! ```ignore
//! pub unsafe extern "C" my_libcall_takes_ref(raw_extern_ref: *mut u8) {
//!     // Before `clone_from_raw`, `raw_extern_ref` is potentially unrooted,
//!     // and doing GC here could lead to use after free!
//!
//!     let my_extern_ref = if raw_extern_ref.is_null() {
//!         None
//!     } else {
//!         Some(VMExternRef::clone_from_raw(raw_extern_ref))
//!     };
//!
//!     // Now that we did `clone_from_raw`, it is safe to do a GC (or do
//!     // anything else that might transitively GC, like call back into
//!     // Wasm!)
//! }
//! ```

#[cfg(all(feature = "gc", feature = "transaction"))]
use crate::ExternRef;
use crate::bail_bug;
use crate::prelude::*;
use crate::runtime::store::{Asyncness, AutoAssertNoGc, InstanceId, StoreOpaque};
#[cfg(feature = "gc")]
use crate::runtime::transaction::DurableExternRefHostData;
#[cfg(all(test, feature = "transaction-mvcc"))]
use crate::runtime::transaction::MvccCommitFaultPoint;
#[cfg(not(feature = "transaction-mvcc"))]
use crate::runtime::transaction::PendingCommitLogEntry;
#[cfg(all(feature = "gc", feature = "transaction"))]
use crate::runtime::transaction::TransactionExternalizedRefHostData;
#[cfg(not(feature = "transaction-mvcc"))]
use crate::runtime::transaction::collect_tmemory_access_versions;
use crate::runtime::transaction::{
    DurableReferenceRegistry, GlobalSnapshot, GranuleId, OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC, OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_I31, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED, OBJECT_VALUE_ABI_TAG_REF, ObjectKind, ObjectTable,
    ObjectValue, ObjectValueAbi, OrdinaryGcPromotionAdapter, OrdinaryGcPromotionSource,
    OrdinaryGcPromotionValue, PERSISTENT_OBJECT_ABI_SLOT_SIZE, StagedRecord, TMemoryAccessSnapshot,
    TMemoryBackend, TableElementSnapshot, TableGranuleSnapshot, TransactionId, TransactionState,
    WasmtimePersistentFieldLayout, WasmtimePersistentFieldLayoutAbi,
    collect_tmemory_access_snapshot, combine_operation_and_cleanup_results,
};
#[cfg(feature = "transaction-mvcc")]
use crate::runtime::transaction::{
    InstalledDomainKeys, MvccRuntime, MvccTerminalCommitState, MvccTerminalDecision,
    PreparedDomainValues, PreparedObjectValue, PreparedValue, TMEMORY_GRANULE_SIZE,
    TMemoryGranuleSnapshot,
};
use crate::runtime::vm::VMGcRef;
use crate::runtime::vm::{
    self, FuncRefTableId, GcStore, HostResultHasUnwindSentinel, TableElementType, VMStore, f32x4,
    f64x2, i8x16,
};
#[cfg(feature = "gc")]
use crate::{ArrayType, StructType};
use crate::{Engine, HeapType, StorageType, ValType};
use alloc::collections::BTreeMap;
#[cfg(feature = "transaction-mvcc")]
use alloc::collections::BTreeSet;
#[cfg(feature = "transaction-mvcc")]
use alloc::sync::Arc;
use core::convert::Infallible;
use core::ptr::NonNull;
#[cfg(feature = "threads")]
use core::time::Duration;
use wasmtime_core::math::WasmFloat;
use wasmtime_environ::{
    CompiledTrap, DefinedMemoryIndex, DefinedTableIndex, FuncIndex, PassiveElemIndex,
    PassiveTElemIndex, TGlobalIndex, TMemoryIndex, TableIndex, Trap, VMGcKind, VMSharedTypeIndex,
    WasmHeapTopType, WasmValType,
};
#[cfg(feature = "gc")]
use wasmtime_environ::{GcLayout, TypeIndex};
#[cfg(feature = "wmemcheck")]
use wasmtime_wmemcheck::AccessError::{
    DoubleMalloc, InvalidFree, InvalidRead, InvalidWrite, OutOfBounds,
};

/// Raw functions which are actually called from compiled code.
///
/// Invocation of a builtin currently looks like:
///
/// * A wasm function calls a cranelift-compiled trampoline that's generated
///   once-per-builtin.
/// * The cranelift-compiled trampoline performs any necessary actions to exit
///   wasm, such as dealing with fp/pc/etc.
/// * The cranelift-compiled trampoline loads a function pointer from an array
///   stored in `VMContext` That function pointer is defined in this module.
/// * This module runs, handling things like `catch_unwind` and `Result` and
///   such.
/// * This module delegates to the outer module (this file) which has the actual
///   implementation.
///
/// For more information on converting from host-defined values to Cranelift ABI
/// values see the `catch_unwind_and_record_trap` function.
pub mod raw {
    use crate::runtime::vm::{Instance, VMContext, f32x4, f64x2, i8x16};
    use core::ptr::NonNull;

    macro_rules! libcall {
        (
            $(
                $( #[cfg($attr:meta)] )?
                $name:ident( vmctx: vmctx $(, $pname:ident: $param:ident )* ) $(-> $result:ident)?;
            )*
        ) => {
            $(
                // This is the direct entrypoint from the compiled module which
                // still has the raw signature.
                //
                // This will delegate to the outer module to the actual
                // implementation and automatically perform `catch_unwind` along
                // with conversion of the return value in the face of traps.
                #[allow(improper_ctypes_definitions, reason = "__m128i known not FFI-safe")]
                #[allow(unused_variables, reason = "macro-generated")]
                #[allow(unreachable_code, reason = "some types uninhabited on some platforms")]
                pub unsafe extern "C" fn $name(
                    vmctx: NonNull<VMContext>,
                    $( $pname : libcall!(@ty $param), )*
                ) $(-> libcall!(@ty $result))? {
                    $(#[cfg($attr)])?
                    unsafe {
                        Instance::enter_host_from_wasm(vmctx, |store, instance| {
                            super::$name(store, instance, $($pname),*)
                        })
                    }
                    $(
                        #[cfg(not($attr))]
                        {
                            let _ = vmctx;
                            unreachable!();
                        }
                    )?
                }

                // This works around a `rustc` bug where compiling with LTO
                // will sometimes strip out some of these symbols resulting
                // in a linking failure.
                #[allow(improper_ctypes_definitions, reason = "__m128i known not FFI-safe")]
                const _: () = {
                    #[used]
                    static I_AM_USED: unsafe extern "C" fn(
                        NonNull<VMContext>,
                        $( $pname : libcall!(@ty $param), )*
                    ) $( -> libcall!(@ty $result))? = $name;
                };
            )*
        };

        (@ty u32) => (u32);
        (@ty u64) => (u64);
        (@ty f32) => (f32);
        (@ty f64) => (f64);
        (@ty u8) => (u8);
        (@ty i8x16) => (i8x16);
        (@ty f32x4) => (f32x4);
        (@ty f64x2) => (f64x2);
        (@ty bool) => (bool);
        (@ty pointer) => (*mut u8);
        (@ty size) => (usize);
    }

    wasmtime_environ::foreach_builtin_function!(libcall);
}

/// Uses the `$store` provided to invoke the async closure `$f` and block on the
/// result.
///
/// This will internally multiplex on `$store.with_blocking(...)` vs simply
/// asserting the closure is ready depending on whether a store's
/// `can_block` flag is set or not.
///
/// FIXME: ideally this would be a function, not a macro. If this is a function
/// though it would require placing a bound on the async closure $f where the
/// returned future is itself `Send`. That's not possible in Rust right now,
/// unfortunately.
///
/// As a workaround this takes advantage of the fact that we can assume that the
/// compiler can infer that the future returned by `$f` is indeed `Send` so long
/// as we don't try to name the type or place it behind a generic. In the future
/// when we can bound the return future of async functions with `Send` this
/// macro should be replaced with an equivalent function.
macro_rules! block_on {
    ($store:expr, $f:expr) => {{
        let store: &mut StoreOpaque = $store;
        let closure = assert_async_fn_closure($f);

        if store.can_block() {
            // If the store can block then that means it's on a fiber. We can
            // forward to `block_on` and everything should be fine and dandy.
            #[cfg(feature = "async")]
            {
                store.with_blocking(|store, cx| cx.block_on(closure(store, Asyncness::Yes)))
            }
            #[cfg(not(feature = "async"))]
            {
                unreachable!()
            }
        } else {
            // If the store cannot block it's not on a fiber. That means that we get
            // at most one poll of `closure(store)` here. In the typical case
            // what this means is that nothing async is configured in the store
            // and one poll should be all we need. There are niche cases where
            // one poll is not sufficient though, for example:
            //
            // * Store is created.
            // * Wasm is called.
            // * Wasm calls host.
            // * Host configures an async resource limiter, returns back to
            //   wasm.
            // * Wasm grows memory.
            // * Limiter wants to block asynchronously.
            //
            // Technically there's nothing wrong with this, but it means that
            // we're in wasm and one poll is not enough here. Given the niche
            // nature of this scenario and how it's not really expected to work
            // this translates failures in `closure` to a trap. This trap is
            // only expected to show up in niche-ish scenarios, not for actual
            // blocking work, as that would otherwise be too surprising.
            vm::one_poll(closure(store, Asyncness::No)).ok_or_else(|| {
                crate::format_err!(
                    "

A synchronously called wasm function invoked an async-defined libcall which
failed to complete synchronously and is thus raising a trap. It's expected
that this indicates that the store was configured to do async things after the
original synchronous entrypoint to wasm was called. That's generally not
supported in Wasmtime and async entrypoint should be used instead. If you're
seeing this message in error please file an issue on Wasmtime.

"
                )
            })
        }
    }};
}

fn assert_async_fn_closure<F, R>(f: F) -> F
where
    F: AsyncFnOnce(&mut StoreOpaque, Asyncness) -> R,
{
    f
}

fn memory_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    delta: u64,
    memory_index: u32,
) -> Result<Option<AllocationSize>> {
    let memory_index = DefinedMemoryIndex::from_u32(memory_index);
    let (mut limiter, store) = store.resource_limiter_and_store_opaque();
    let limiter = limiter.as_mut();
    block_on!(store, async |store, _| {
        let instance = store.instance_mut(instance);
        let module = instance.env_module();
        let page_size_log2 = module.memories[module.memory_index(memory_index)].page_size_log2;

        let result = instance
            .memory_grow(limiter, memory_index, delta)
            .await?
            .map(|size_in_bytes| AllocationSize(size_in_bytes >> page_size_log2));

        Ok(result)
    })?
}

// Transaction libcalls execute compiled transactional operators against
// store-local transaction state and per-instance `tmemory` sidecars.
fn transaction_enter_tfunc(store: &mut dyn VMStore, instance: InstanceId) -> Result<u32> {
    #[cfg(feature = "transaction-mvcc")]
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .has_mvcc_terminal_commit()
    {
        transaction_commit_mvcc_impl(store, instance)?;
    }
    #[cfg(not(feature = "transaction-mvcc"))]
    let _ = instance;

    let store = store.store_opaque_mut();
    let region = store.transaction_region_runtime().clone();
    let state = store.transaction_state_mut();
    if state.structured_failure_pending() {
        return Ok(2);
    }
    if state.active_transaction().is_some() {
        return Ok(u32::from(state.take_active_tfunc_ownership()));
    }
    state.begin_with_region_runtime(&region)?;
    Ok(1)
}

fn transaction_enter_tblock(store: &mut dyn VMStore, _instance: InstanceId) -> Result<u32> {
    let store = store.store_opaque_mut();
    let region = store.transaction_region_runtime().clone();
    let state = store.transaction_state_mut();
    if state.structured_failure_pending() {
        return Ok(2);
    }
    if state.active_transaction().is_some() {
        return Ok(0);
    }
    state.begin_with_region_runtime(&region)?;
    Ok(1)
}

fn transaction_active(store: &mut dyn VMStore, _instance: InstanceId) -> u32 {
    u32::from(
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .active_transaction()
            .is_some(),
    )
}

fn transaction_transfer_tfunc_ownership(
    store: &mut dyn VMStore,
    _instance: InstanceId,
) -> Result<()> {
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .transfer_active_tfunc_ownership()
}

fn transaction_start_tfunc_tail(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .request_tfunc_tail_start()
}

fn transaction_claim_tfunc_tail(store: &mut dyn VMStore, _instance: InstanceId) -> Result<u32> {
    let store = store.store_opaque_mut();
    let region = store.transaction_region_runtime().clone();
    store
        .transaction_state_mut()
        .claim_tfunc_tail_for_host(&region)
        .map(u32::from)
}

fn transaction_begin(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    #[cfg(feature = "transaction-mvcc")]
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .has_mvcc_terminal_commit()
    {
        transaction_commit_mvcc_impl(store, instance)?;
    }
    #[cfg(not(feature = "transaction-mvcc"))]
    let _ = instance;

    let store = store.store_opaque_mut();
    let region = store.transaction_region_runtime().clone();
    let state = store.transaction_state_mut();
    if state.structured_failure_pending() {
        return Ok(());
    }
    state.begin_with_region_runtime(&region)?;
    Ok(())
}

fn transaction_ttry_end(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    let state = store.store_opaque_mut().transaction_state_mut();
    if state.structured_failure_pending() {
        state.clear_structured_failure();
        return Ok(());
    }
    transaction_commit_impl(store, instance)
}

fn transaction_commit(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    let result = transaction_commit_impl(store, instance);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_preserve_tref_result(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    raw_ref: u32,
) -> Result<()> {
    transaction_preserve_tref_result_impl(store.store_opaque_mut(), raw_ref)
}

fn transaction_preserve_textern_result(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    raw_ref: u32,
) -> Result<()> {
    let result = (|| {
        if raw_ref == 0 {
            return Ok(());
        }
        #[cfg(all(feature = "gc", feature = "transaction"))]
        if let Some(raw_ref) = transaction_externalized_ref_raw(store.store_opaque(), raw_ref)? {
            return transaction_preserve_tref_result_impl(store.store_opaque_mut(), raw_ref);
        }
        Ok(())
    })();
    let cleanup = abort_active_transaction_on_error(store, &result);
    combine_operation_and_cleanup_results(
        result,
        cleanup,
        "failed to abort transaction after externalized result promotion failure",
    )
}

#[cfg(all(feature = "gc", feature = "transaction"))]
fn transaction_externalized_ref_raw(store: &StoreOpaque, raw_ref: u32) -> Result<Option<u32>> {
    let gc_ref = VMGcRef::from_raw_u32(raw_ref).context("invalid transactional externref")?;
    let gc_store = store.require_gc_store()?;
    let extern_ref = gc_ref
        .as_externref(&*gc_store.gc_heap)
        .context("transactional extern value is not an externref")?;
    Ok(gc_store
        .externref_host_data(extern_ref)?
        .downcast_ref::<TransactionExternalizedRefHostData>()
        .map(TransactionExternalizedRefHostData::raw_ref))
}

fn transaction_textern_convert_tany(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    raw_ref: u32,
) -> Result<u32> {
    #[cfg(not(all(feature = "gc", feature = "transaction")))]
    let _ = &store;
    if raw_ref == 0 {
        return Ok(0);
    }

    #[cfg(feature = "transaction")]
    {
        let mut scope = crate::OpaqueRootScope::new(store.store_opaque_mut());
        if let Some(reference) = scope.transaction_extern_for_handle(raw_ref) {
            let mut no_gc = AutoAssertNoGc::new(&mut **scope);
            return reference._to_raw(&mut no_gc);
        }
    }

    #[cfg(not(all(feature = "gc", feature = "transaction")))]
    bail!("transactional external conversion requires GC support");
    #[cfg(all(feature = "gc", feature = "transaction"))]
    {
        let (mut limiter, store) = store.resource_limiter_and_store_opaque();
        let mut scope = crate::OpaqueRootScope::new(store);
        let reference = block_on!(&mut **scope, async |store, asyncness| {
            ExternRef::_new_async(
                store,
                limiter.as_mut(),
                TransactionExternalizedRefHostData::new(raw_ref),
                asyncness,
            )
            .await
        })??;
        let mut no_gc = AutoAssertNoGc::new(&mut **scope);
        reference._to_raw(&mut no_gc)
    }
}

fn transaction_tany_convert_textern(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    raw_ref: u32,
) -> Result<u32> {
    #[cfg(not(all(feature = "gc", feature = "transaction")))]
    let _ = &store;
    if raw_ref == 0 {
        return Ok(0);
    }

    #[cfg(not(all(feature = "gc", feature = "transaction")))]
    bail!("transactional external conversion requires GC support");
    #[cfg(all(feature = "gc", feature = "transaction"))]
    {
        if let Some(raw) = transaction_externalized_ref_raw(store.store_opaque(), raw_ref)? {
            return Ok(raw);
        }
        let reference = {
            let mut no_gc = AutoAssertNoGc::new(store.store_opaque_mut());
            ExternRef::_from_raw(&mut no_gc, raw_ref).context("null transactional externref")?
        };
        let handle = store
            .store_opaque_mut()
            .transaction_extern_handle(reference)?;
        Ok(handle)
    }
}

pub(crate) fn transaction_preserve_tref_result_impl(
    store: &mut StoreOpaque,
    raw_ref: u32,
) -> Result<()> {
    let result = (|| {
        if raw_ref == 0 || ObjectTable::is_raw_i31_ref(u64::from(raw_ref)) {
            return Ok(());
        }

        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let Some(object_id) =
            state.known_object_id_for_transaction_ref_handle(object_table, raw_ref)
        else {
            // Transactional external identities share the `tany` hierarchy but
            // are not backed by transaction object records.
            return Ok(());
        };
        state.promote_transaction_object_graph(object_table, object_id)?;
        Ok(())
    })();
    let cleanup = if result.is_err() && store.transaction_state().active_transaction().is_some() {
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.abort_allocated_objects(object_table)
    } else {
        Ok(())
    };
    combine_operation_and_cleanup_results(
        result,
        cleanup,
        "failed to abort transaction after result promotion failure",
    )
}

fn transaction_commit_structured(store: &mut dyn VMStore, instance: InstanceId) -> Result<u32> {
    const CONFLICT_MARKERS: &[&str] = &[
        "transaction read conflict",
        "transaction write conflict",
        "transaction conflict would wait",
        "transaction was conflict-aborted",
        "transaction MVCC certification conflict",
    ];

    let result = transaction_commit_impl(store, instance);
    let conflict = result.as_ref().is_err_and(|error| {
        let mut conflict = false;
        for cause in error.chain() {
            let message = cause.to_string();
            if message.contains("failed to abort") {
                return false;
            }
            conflict |= CONFLICT_MARKERS
                .iter()
                .any(|marker| message.contains(marker));
        }
        conflict
    });
    let cleanup = abort_active_transaction_on_error(store, &result);
    if conflict {
        cleanup.context("failed to abort structured transaction after commit conflict")?;
        return Ok(1);
    }
    combine_operation_and_cleanup_results(
        result,
        cleanup,
        "failed to abort structured transaction after commit failure",
    )?;
    Ok(0)
}

pub(crate) fn transaction_commit_selected_for_host(
    store: &mut dyn VMStore,
    instance: InstanceId,
    transaction: TransactionId,
) -> Result<bool> {
    let previous = {
        let state = store.store_opaque_mut().transaction_state_mut();
        if !state.transaction_is_open(transaction) {
            return Ok(false);
        }
        if state.active_transaction() == Some(transaction) {
            None
        } else {
            state.enter_transaction(transaction)?
        }
    };

    let result = transaction_commit_impl(store, instance);
    let _ = abort_active_transaction_on_error(store, &result);
    let restore_result = store
        .store_opaque_mut()
        .transaction_state_mut()
        .restore_transaction(previous);

    result?;
    restore_result?;
    Ok(true)
}

#[cfg_attr(not(feature = "gc"), allow(dead_code))]
struct StoreBackedOrdinaryGcPromotionAdapter<'a> {
    engine: &'a Engine,
    gc_store: Option<&'a mut GcStore>,
    durable_refs: &'a mut DurableReferenceRegistry,
}

#[cfg_attr(not(feature = "gc"), allow(dead_code))]
impl<'a> StoreBackedOrdinaryGcPromotionAdapter<'a> {
    fn new(
        engine: &'a Engine,
        gc_store: Option<&'a mut GcStore>,
        durable_refs: &'a mut DurableReferenceRegistry,
    ) -> Self {
        Self {
            engine,
            gc_store,
            durable_refs,
        }
    }

    fn gc_store_mut(&mut self) -> Result<&mut GcStore> {
        self.gc_store
            .as_deref_mut()
            .context("ordinary Wasmtime GC promotion requires a GC store")
    }

    fn read_storage_value(
        &mut self,
        gc_ref: &VMGcRef,
        ty: &StorageType,
        offset: u32,
    ) -> Result<OrdinaryGcPromotionValue> {
        let data = self.gc_store_mut()?.gc_object_data(gc_ref)?;
        Ok(match ty {
            StorageType::I8 => OrdinaryGcPromotionValue::I32(i32::from(data.read_u8(offset)?)),
            StorageType::I16 => OrdinaryGcPromotionValue::I32(i32::from(data.read_u16(offset)?)),
            StorageType::ValType(ValType::I32) => {
                OrdinaryGcPromotionValue::I32(data.read_i32(offset)?)
            }
            StorageType::ValType(ValType::I64) => {
                OrdinaryGcPromotionValue::I64(data.read_i64(offset)?)
            }
            StorageType::ValType(ValType::F32) => {
                OrdinaryGcPromotionValue::F32(data.read_u32(offset)?)
            }
            StorageType::ValType(ValType::F64) => {
                OrdinaryGcPromotionValue::F64(data.read_u64(offset)?)
            }
            StorageType::ValType(ValType::V128) => {
                OrdinaryGcPromotionValue::V128(data.read_v128(offset)?.as_u128().to_le_bytes())
            }
            StorageType::ValType(ValType::Ref(ref_type)) => {
                let raw = data.read_u32(offset)?;
                match ref_type.heap_type() {
                    HeapType::Func | HeapType::ConcreteFunc(_) | HeapType::NoFunc => {
                        return self.read_func_ref_value(raw);
                    }
                    HeapType::Extern | HeapType::NoExtern => {
                        return self.read_extern_ref_value(raw);
                    }
                    HeapType::Cont | HeapType::ConcreteCont(_) | HeapType::NoCont => {
                        bail!("ordinary GC promotion cannot encode continuation references durably")
                    }
                    HeapType::Exn | HeapType::ConcreteExn(_) | HeapType::NoExn => {
                        bail!("ordinary GC promotion cannot encode exception references durably")
                    }
                    HeapType::Any
                    | HeapType::Eq
                    | HeapType::I31
                    | HeapType::Array
                    | HeapType::ConcreteArray(_)
                    | HeapType::Struct
                    | HeapType::ConcreteStruct(_)
                    | HeapType::None => {
                        return self.read_generic_gc_ref_value(raw);
                    }
                }
            }
        })
    }

    fn read_generic_gc_ref_value(&mut self, raw_gc_ref: u32) -> Result<OrdinaryGcPromotionValue> {
        if raw_gc_ref == 0 {
            return Ok(OrdinaryGcPromotionValue::GcRef(None));
        }
        let Some(gc_ref) = VMGcRef::from_raw_u32(raw_gc_ref) else {
            bail!("ordinary GC promotion cannot encode invalid generic reference");
        };
        if gc_ref.is_i31() {
            return Ok(OrdinaryGcPromotionValue::I31(
                ObjectTable::decode_raw_i31_ref(u64::from(raw_gc_ref))?,
            ));
        }
        if self.durable_refs.resolve_extern_ref(raw_gc_ref).is_some() {
            return self.read_extern_ref_value(raw_gc_ref);
        }
        #[cfg(feature = "gc")]
        if let Some(gc_store) = self.gc_store.as_mut()
            && gc_ref.as_externref(&*gc_store.gc_heap).is_some()
        {
            return self.read_extern_ref_value(raw_gc_ref);
        }
        Ok(OrdinaryGcPromotionValue::GcRef(Some(raw_gc_ref)))
    }

    fn read_func_ref_value(&mut self, raw_func_ref_id: u32) -> Result<OrdinaryGcPromotionValue> {
        let vm_func_ref_addr = {
            let func_ref_id = FuncRefTableId::from_raw(raw_func_ref_id);
            let Some(func_ref) = self
                .gc_store_mut()?
                .func_ref_table
                .get_untyped(func_ref_id)?
            else {
                return Ok(OrdinaryGcPromotionValue::GcRef(None));
            };
            func_ref.as_non_null().as_ptr().addr()
        };
        let identity = self
            .durable_refs
            .resolve_or_register_live_func_ref(vm_func_ref_addr)?;
        Ok(OrdinaryGcPromotionValue::FuncRef(identity))
    }

    fn read_extern_ref_value(&mut self, raw_gc_ref: u32) -> Result<OrdinaryGcPromotionValue> {
        if raw_gc_ref == 0 {
            return Ok(OrdinaryGcPromotionValue::GcRef(None));
        }
        let Some(gc_ref) = VMGcRef::from_raw_u32(raw_gc_ref) else {
            bail!("ordinary GC promotion cannot encode invalid external reference");
        };
        #[cfg(not(feature = "gc"))]
        let _ = gc_ref;
        #[cfg(feature = "gc")]
        let embedded_identity = {
            let gc_store = self.gc_store_mut()?;
            let Some(extern_ref) = gc_ref.as_externref(&*gc_store.gc_heap) else {
                bail!("ordinary GC promotion expected an external reference");
            };
            gc_store
                .externref_host_data(extern_ref)?
                .downcast_ref::<DurableExternRefHostData>()
                .map(DurableExternRefHostData::identity)
        };
        #[cfg(not(feature = "gc"))]
        let embedded_identity = None;
        let identity = match embedded_identity {
            Some(identity) => {
                self.durable_refs
                    .register_extern_ref(raw_gc_ref, identity)?;
                identity
            }
            None => self
                .durable_refs
                .resolve_or_register_live_extern_ref(raw_gc_ref)?,
        };
        Ok(OrdinaryGcPromotionValue::ExternRef(identity))
    }

    fn storage_type_traces_object_refs(ty: &StorageType) -> bool {
        match ty {
            StorageType::I8 | StorageType::I16 => false,
            StorageType::ValType(ty) => match ty {
                ValType::Ref(ref_type) => matches!(
                    ref_type.heap_type(),
                    HeapType::Any
                        | HeapType::Eq
                        | HeapType::Array
                        | HeapType::ConcreteArray(_)
                        | HeapType::Struct
                        | HeapType::ConcreteStruct(_)
                ),
                ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64 | ValType::V128 => false,
            },
        }
    }

    fn canonical_field_layouts(
        field_types: &[StorageType],
    ) -> Result<Vec<WasmtimePersistentFieldLayout>> {
        field_types
            .iter()
            .enumerate()
            .map(|(index, ty)| {
                let field_index =
                    u32::try_from(index).context("ordinary GC struct field index overflow")?;
                let field_offset = field_index
                    .checked_mul(PERSISTENT_OBJECT_ABI_SLOT_SIZE)
                    .context("ordinary GC struct field offset overflow")?;
                Ok(WasmtimePersistentFieldLayout {
                    field_index,
                    field_offset,
                    value_size: PERSISTENT_OBJECT_ABI_SLOT_SIZE,
                    is_object_ref: Self::storage_type_traces_object_refs(ty),
                })
            })
            .collect()
    }

    fn promote_struct_source(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: &VMGcRef,
        type_index: VMSharedTypeIndex,
    ) -> Result<OrdinaryGcPromotionSource> {
        #[cfg(not(feature = "gc"))]
        {
            let _ = (object_table, gc_ref, type_index);
            bail!("ordinary GC struct promotion requires the `gc` cargo feature");
        }
        #[cfg(feature = "gc")]
        {
            let layout = self
                .engine
                .signatures()
                .layout(type_index)
                .with_context(|| {
                    format!(
                        "ordinary GC struct type {} has no registered GC layout",
                        type_index.bits()
                    )
                })?;
            let GcLayout::Struct(layout) = layout else {
                bail!("ordinary GC struct type layout is not a struct layout");
            };
            let field_types = StructType::from_shared_type_index(self.engine, type_index)
                .fields()
                .map(|field| field.element_type().clone())
                .collect::<Vec<_>>();
            ensure!(
                field_types.len() == layout.fields.len(),
                "ordinary GC struct layout field count mismatch"
            );
            let mut fields = Vec::with_capacity(field_types.len());
            for (field_type, field_layout) in field_types.iter().zip(layout.fields.iter()) {
                fields.push(self.read_storage_value(gc_ref, field_type, field_layout.offset)?);
            }
            let field_layouts = Self::canonical_field_layouts(&field_types)?;
            let type_layout_id = object_table
                .ensure_persistent_struct_layout_for_wasmtime_type_layout_namespace(
                    // VMSharedTypeIndex is engine-global, so ordinary Wasmtime GC
                    // promotion uses the stable engine-level namespace.
                    0,
                    type_index.bits(),
                    field_layouts,
                )?;
            Ok(OrdinaryGcPromotionSource::Struct {
                type_layout_id,
                fields,
            })
        }
    }

    fn promote_array_source(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: &VMGcRef,
        type_index: VMSharedTypeIndex,
    ) -> Result<OrdinaryGcPromotionSource> {
        #[cfg(not(feature = "gc"))]
        {
            let _ = (object_table, gc_ref, type_index);
            bail!("ordinary GC array promotion requires the `gc` cargo feature");
        }
        #[cfg(feature = "gc")]
        {
            let layout = self
                .engine
                .signatures()
                .layout(type_index)
                .with_context(|| {
                    format!(
                        "ordinary GC array type {} has no registered GC layout",
                        type_index.bits()
                    )
                })?;
            let GcLayout::Array(layout) = layout else {
                bail!("ordinary GC array type layout is not an array layout");
            };
            let element_type =
                ArrayType::from_shared_type_index(self.engine, type_index).element_type();
            let len = {
                let gc_store = self.gc_store_mut()?;
                let array_ref = gc_ref
                    .as_arrayref(&*gc_store.gc_heap)
                    .context("ordinary GC ref is not an arrayref")?;
                gc_store.array_len(array_ref)?
            };
            let mut elements = Vec::with_capacity(
                usize::try_from(len).context("ordinary GC array length exceeds usize")?,
            );
            for index in 0..len {
                let offset = layout
                    .elem_offset(index)
                    .context("ordinary GC array element offset overflow")?;
                elements.push(self.read_storage_value(gc_ref, &element_type, offset)?);
            }
            let element_is_object_ref = Self::storage_type_traces_object_refs(&element_type);
            let type_layout_id = object_table
                .ensure_persistent_array_layout_for_wasmtime_type_layout_namespace(
                    // VMSharedTypeIndex is engine-global, so ordinary Wasmtime GC
                    // promotion uses the stable engine-level namespace.
                    0,
                    type_index.bits(),
                    PERSISTENT_OBJECT_ABI_SLOT_SIZE,
                    element_is_object_ref,
                )?;
            Ok(OrdinaryGcPromotionSource::Array {
                type_layout_id,
                elements,
            })
        }
    }
}

impl OrdinaryGcPromotionAdapter for StoreBackedOrdinaryGcPromotionAdapter<'_> {
    fn promotion_source_for_gc_ref(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: u32,
    ) -> Result<Option<OrdinaryGcPromotionSource>> {
        let Some(gc_ref) = VMGcRef::from_raw_u32(gc_ref) else {
            return Ok(None);
        };
        if gc_ref.is_i31() {
            return Ok(Some(OrdinaryGcPromotionSource::I31(
                ObjectTable::decode_raw_i31_ref(u64::from(gc_ref.as_raw_u32()))?,
            )));
        }
        let Some(gc_store) = self.gc_store.as_mut() else {
            return Ok(None);
        };
        let (kind, type_index) = {
            let header = match gc_store.header(&gc_ref) {
                Ok(header) => header,
                Err(_) => {
                    return Ok(Some(OrdinaryGcPromotionSource::Unsupported(
                        "ordinary GC reference is not valid in this store",
                    )));
                }
            };
            (header.kind(), header.ty())
        };
        if kind.matches(VMGcKind::StructRef) {
            let type_index = type_index.context("ordinary GC struct has no concrete type")?;
            return self
                .promote_struct_source(object_table, &gc_ref, type_index)
                .map(Some);
        }
        if kind.matches(VMGcKind::ArrayRef) {
            let type_index = type_index.context("ordinary GC array has no concrete type")?;
            return self
                .promote_array_source(object_table, &gc_ref, type_index)
                .map(Some);
        }
        if kind.matches(VMGcKind::ExternRef) {
            let OrdinaryGcPromotionValue::ExternRef(identity) =
                self.read_extern_ref_value(gc_ref.as_raw_u32())?
            else {
                bail!("ordinary GC promotion expected durable external reference leaf");
            };
            return Ok(Some(OrdinaryGcPromotionSource::ExternRef(identity)));
        }
        Ok(Some(OrdinaryGcPromotionSource::Unsupported(
            "ordinary GC object kind is not supported for persistent promotion",
        )))
    }
}

fn transaction_commit_impl(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    {
        let state = store.store_opaque_mut().transaction_state_mut();
        if state.structured_failure_pending() {
            state.clear_structured_failure();
            return Ok(());
        }
        if state.active_transaction().is_none() {
            return Ok(());
        }
    }

    #[cfg(not(feature = "transaction-mvcc"))]
    return transaction_commit_single_version_impl(store, instance);
    #[cfg(feature = "transaction-mvcc")]
    return transaction_commit_mvcc_impl(store, instance);
}

#[cfg(not(feature = "transaction-mvcc"))]
fn transaction_commit_single_version_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
) -> Result<()> {
    {
        let store = store.store_opaque_mut();
        let (engine, gc_store, durable_refs, state, object_table) =
            store.transaction_promotion_context_mut();
        let mut adapter =
            StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state
            .promote_persistent_references_before_commit_with_adapter(object_table, &mut adapter)?;
    }

    let _user_transaction_region_permit = {
        let state = store.store_opaque_mut().transaction_state_mut();
        if let Some(runtime) = state.shared_region_runtime_for_publication() {
            Some(runtime.begin_user_transaction_region()?)
        } else {
            None
        }
    };

    #[cfg(not(feature = "transaction-cc-strict-2pl"))]
    let _certification = store
        .store_opaque_mut()
        .transaction_state_mut()
        .acquire_active_optimistic_certification()?;

    let result = transaction_commit_single_version_certified(store, instance);
    let cleanup = abort_active_transaction_on_error(store, &result);
    combine_operation_and_cleanup_results(
        result,
        cleanup,
        "failed to abort single-version transaction after certified commit failure",
    )
}

#[cfg(not(feature = "transaction-mvcc"))]
fn transaction_commit_single_version_certified(
    store: &mut dyn VMStore,
    instance: InstanceId,
) -> Result<()> {
    let (records, read_granules, write_granules) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        (
            state.staged_records()?,
            state.active_read_granules()?,
            state.active_write_granules()?,
        )
    };

    for granule in read_granules {
        let current_version = current_granule_version(store, instance, granule)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .validate_active_read(granule, current_version)?;
    }
    for granule in write_granules {
        let current_version = current_granule_version(store, instance, granule)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .validate_active_write(granule, current_version)?;
    }

    let (stream_id, txid) = store
        .store_opaque_mut()
        .transaction_state_mut()
        .durable_publication_ids()?;

    {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.begin_terminal_commit_with_object_cleanup(object_table)?;
    }

    let mut final_marker =
        commit_staged_tmemory_records(store, instance, &records, stream_id, txid)?;

    for record in &records {
        apply_staged_transaction_record(store, instance, record)?;
    }

    let (durable_publications, root_delta, persistent_gc_delta) = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let mut object_publications = Vec::new();
        state.commit_object_payloads_into(object_table, &mut object_publications)?;
        let root_delta = state.staged_persistent_root_delta(&*object_table)?;
        let root_publications = state.persistent_root_publications(&root_delta)?;
        let tmemory_size_publications = state.tmemory_size_publications()?;
        let persistent_gc_delta =
            state.persistent_gc_commit_delta(&*object_table, &object_publications)?;
        object_publications.extend(root_publications);
        object_publications.extend(tmemory_size_publications);
        (object_publications, root_delta, persistent_gc_delta)
    };
    let mut durable_publication_markers = Vec::new();
    if !durable_publications.is_empty() {
        durable_publication_markers = {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state.publish_object_publications_before_commit_with_markers(
                stream_id,
                txid,
                &*object_table,
                &durable_publications,
            )?
        };
        final_marker = durable_publication_markers.last().copied().or(final_marker);
    }
    let mut lp_published = false;
    if let Some(marker) = final_marker {
        if store
            .store_opaque_mut()
            .transaction_state_mut()
            .take_fail_next_commit_before_lp_for_test()
        {
            bail!("transaction test failure before commit LP");
        }
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .publish_commit_lp(stream_id, txid, marker)?;
        lp_published = true;
    }

    let commit_result = store
        .store_opaque_mut()
        .transaction_state_mut()
        .complete_commit_with_persistent_root_delta(root_delta);
    if let Err(error) = commit_result {
        if lp_published {
            let object_install_result = if durable_publication_markers.is_empty() {
                Ok(())
            } else {
                let store = store.store_opaque_mut();
                let (state, object_table) = store.transaction_state_and_object_table_mut();
                state
                    .install_committed_mapped_object_publications(
                        object_table,
                        &durable_publications,
                        &durable_publication_markers,
                    )
                    .map(|_| ())
            };
            let cleanup_result = store
                .store_opaque_mut()
                .transaction_state_mut()
                .finish_committed_cleanup_after_durable_commit_error();
            if let Err(install_error) = object_install_result {
                return match cleanup_result {
                    Ok(()) => Err(install_error.context(format!(
                        "transaction committed durably but object publication install after commit failure failed: {error}"
                    ))),
                    Err(cleanup_error) => Err(cleanup_error.context(format!(
                        "transaction committed durably but object publication install and cleanup after commit failure also failed: {error}; object install error: {install_error}"
                    ))),
                };
            }
            return match cleanup_result {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error.context(format!(
                    "transaction committed durably but cleanup after commit failure also failed: {error}"
                ))),
            };
        }
        return Err(error);
    }

    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    if !durable_publication_markers.is_empty() {
        state.install_committed_mapped_object_publications(
            object_table,
            &durable_publications,
            &durable_publication_markers,
        )?;
    }
    // The transaction is already committed at this point. Persistent GC
    // observation is opportunistic runtime maintenance and must not turn a
    // completed commit into an apparent failure.
    let _ = state.observe_persistent_gc_commit_delta_after_commit_best_effort(
        object_table,
        &persistent_gc_delta,
    );
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn transaction_commit_mvcc_impl(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .has_mvcc_terminal_commit()
    {
        return drive_mvcc_terminal_commit(store);
    }

    {
        let store = store.store_opaque_mut();
        let (engine, gc_store, durable_refs, state, object_table) =
            store.transaction_promotion_context_mut();
        let mut adapter =
            StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state
            .promote_persistent_references_before_commit_with_adapter(object_table, &mut adapter)?;
        state.finalize_transaction_local_objects_after_promotion(object_table)?;
    }

    let (records, visibility, snapshot, transaction, reads, writes) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        let (visibility, snapshot, transaction, reads, writes) =
            state.active_mvcc_commit_context()?;
        (
            state.staged_records()?,
            visibility,
            snapshot,
            transaction,
            reads,
            writes,
        )
    };
    let staged_objects = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.staged_object_payloads_for_commit(&*object_table)?
    };

    let user_transaction_region_permit = {
        let state = store.store_opaque_mut().transaction_state_mut();
        if let Some(runtime) = state.shared_region_runtime_for_publication() {
            Some(runtime.begin_user_transaction_region()?)
        } else {
            None
        }
    };

    if writes.is_empty() {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.begin_terminal_commit_with_object_cleanup(object_table)?;
        return state.complete_commit();
    }

    for granule in writes.iter().copied() {
        let current_version = current_granule_version(store, instance, granule)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .validate_active_write(granule, current_version)?;
    }

    let certification = store
        .store_opaque_mut()
        .transaction_state_mut()
        .acquire_active_mvcc_certification(&visibility, snapshot, transaction, &reads, &writes)?;

    let (commit, pending) = visibility.begin_pending_commit()?;
    #[cfg(test)]
    visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterCertification)?;
    let mut physical_values = PreparedDomainValues::default();
    let mut table_granules = BTreeSet::new();
    for record in &records {
        match record {
            StagedRecord::MemoryGranule {
                owner_instance,
                memory_index,
                granule_index,
                bytes,
            } => {
                let owner = owner_instance.unwrap_or(instance);
                let granule = GranuleId::TMemory {
                    instance: owner_instance.map(InstanceId::as_u32),
                    memory_index: *memory_index,
                    granule_index: *granule_index,
                };
                let current = current_tmemory_granule(
                    store,
                    owner,
                    *memory_index,
                    *granule_index,
                    bytes.len(),
                )?;
                ensure!(
                    physical_values
                        .memories
                        .insert(
                            granule,
                            PreparedValue {
                                predecessor: current,
                                value: bytes.clone(),
                            },
                        )
                        .is_none(),
                    "memory granule was prepared more than once"
                );
            }
            StagedRecord::MemorySize {
                owner_instance,
                memory_index,
                new_pages,
            } => {
                let owner = owner_instance.unwrap_or(instance);
                let granule = GranuleId::TMemorySize {
                    instance: owner_instance.map(InstanceId::as_u32),
                    memory_index: *memory_index,
                };
                let current =
                    current_tmemory_pages(store, owner, TMemoryIndex::from_u32(*memory_index))?;
                ensure!(
                    physical_values
                        .memory_sizes
                        .insert(
                            granule,
                            PreparedValue {
                                predecessor: current,
                                value: *new_pages,
                            },
                        )
                        .is_none(),
                    "memory size was prepared more than once"
                );
            }
            StagedRecord::Global {
                owner_instance,
                global_index,
                value,
            } => {
                let owner = owner_instance.unwrap_or(instance);
                let granule = GranuleId::TGlobal {
                    instance: owner_instance.map(InstanceId::as_u32),
                    global_index: *global_index,
                };
                let current = read_global_snapshot_like(
                    store,
                    owner,
                    TGlobalIndex::from_u32(*global_index),
                    *value,
                )?;
                ensure!(
                    physical_values
                        .globals
                        .insert(
                            granule,
                            PreparedValue {
                                predecessor: current,
                                value: *value,
                            },
                        )
                        .is_none(),
                    "global was prepared more than once"
                );
            }
            StagedRecord::TableSize {
                owner_instance,
                table_index,
                new_elements,
            } => {
                let owner = owner_instance.unwrap_or(instance);
                let granule = GranuleId::TTableSize {
                    instance: owner_instance.map(InstanceId::as_u32),
                    table_index: *table_index,
                };
                let current = u64::try_from(defined_table_size(store, owner, *table_index)?)
                    .context("defined table size does not fit u64")?;
                ensure!(
                    physical_values
                        .table_sizes
                        .insert(
                            granule,
                            PreparedValue {
                                predecessor: current,
                                value: *new_elements,
                            },
                        )
                        .is_none(),
                    "table size was prepared more than once"
                );
            }
            StagedRecord::TableElement {
                owner_instance,
                table_index,
                element_index,
                ..
            } => {
                table_granules.insert((
                    owner_instance.map(InstanceId::as_u32),
                    *table_index,
                    element_index / TableGranuleSnapshot::ELEMENT_CAPACITY,
                ));
            }
        }
    }

    for (owner_instance, table_index, granule_index) in table_granules {
        let owner_instance = owner_instance.map(InstanceId::from_u32);
        let owner = owner_instance.unwrap_or(instance);
        let physical_size = u64::try_from(defined_table_size(store, owner, table_index)?)
            .context("defined table size does not fit u64")?;
        let final_size = store
            .store_opaque_mut()
            .transaction_state_mut()
            .staged_table_size_owned(owner_instance, table_index)
            .unwrap_or(physical_size);
        let predecessor =
            collect_current_table_granule(store, owner, table_index, granule_index, physical_size)?;
        let mut elements = predecessor.elements().to_vec();
        let granule_start = granule_index
            .checked_mul(TableGranuleSnapshot::ELEMENT_CAPACITY)
            .context("ttable granule start overflow")?;
        let final_len = final_size
            .saturating_sub(granule_start)
            .min(TableGranuleSnapshot::ELEMENT_CAPACITY);
        let null = records
            .iter()
            .find_map(|record| match record {
                StagedRecord::TableElement {
                    owner_instance: staged_owner,
                    table_index: staged_table,
                    element_index,
                    value,
                } if *staged_owner == owner_instance
                    && *staged_table == table_index
                    && *element_index / TableGranuleSnapshot::ELEMENT_CAPACITY == granule_index =>
                {
                    Some(match value {
                        TableElementSnapshot::FuncRef(_) => TableElementSnapshot::FuncRef(0),
                        TableElementSnapshot::GcRef(_) => TableElementSnapshot::GcRef(0),
                    })
                }
                _ => None,
            })
            .context("staged table granule has no element value")?;
        elements.resize(
            usize::try_from(final_len).context("ttable granule length does not fit usize")?,
            null,
        );
        for record in &records {
            let StagedRecord::TableElement {
                owner_instance: staged_owner,
                table_index: staged_table,
                element_index,
                value,
            } = record
            else {
                continue;
            };
            if *staged_owner != owner_instance
                || *staged_table != table_index
                || *element_index / TableGranuleSnapshot::ELEMENT_CAPACITY != granule_index
            {
                continue;
            }
            let offset = usize::try_from(*element_index - granule_start)
                .context("ttable granule offset does not fit usize")?;
            elements[offset] = *value;
        }
        let value = TableGranuleSnapshot::new(elements)?;
        let granule = GranuleId::TTable {
            instance: owner_instance.map(InstanceId::as_u32),
            table_index,
            granule_index,
        };
        ensure!(
            physical_values
                .tables
                .insert(granule, PreparedValue { predecessor, value })
                .is_none(),
            "table granule was prepared more than once"
        );
    }

    for (object, value) in staged_objects {
        let newly_allocated = store
            .store_opaque_mut()
            .transaction_state_mut()
            .object_is_newly_allocated_for_commit(object)?;
        let predecessor = if newly_allocated {
            None
        } else {
            let store = store.store_opaque_mut();
            let (_state, object_table) = store.transaction_state_and_object_table_mut();
            object_table.current_payload_snapshot(object)?
        };
        ensure!(
            physical_values
                .objects
                .insert(object, PreparedObjectValue { predecessor, value })
                .is_none(),
            "object was prepared more than once"
        );
    }
    #[cfg(test)]
    visibility.run_predecessor_collected_hook_for_test()?;

    let (stream_id, txid) = store
        .store_opaque_mut()
        .transaction_state_mut()
        .durable_publication_ids()?;

    {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.begin_terminal_commit_with_object_cleanup(object_table)?;
    }

    let (durable_publications, root_delta, persistent_gc_delta, volatile_objects) = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let (mut object_publications, volatile_objects) =
            state.prepare_mvcc_object_payload_publications(object_table)?;
        let root_delta = state.staged_persistent_root_delta(&*object_table)?;
        let root_publications = state.persistent_root_publications(&root_delta)?;
        let tmemory_size_publications = state.tmemory_size_publications()?;
        let persistent_gc_delta =
            state.persistent_gc_commit_delta(&*object_table, &object_publications)?;
        object_publications.extend(root_publications);
        object_publications.extend(tmemory_size_publications);
        (
            object_publications,
            root_delta,
            persistent_gc_delta,
            volatile_objects,
        )
    };
    let mut prepare = visibility.begin_prepare(commit.clone())?;
    prepare.append_prepared_values(physical_values)?;
    let prepared_values = prepare.into_values();
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .install_mvcc_terminal_commit(MvccTerminalCommitState {
            instance,
            stream_id,
            txid,
            commit,
            pending: Some(pending),
            certification: Some(certification),
            user_region_permit: user_transaction_region_permit,
            prepared: prepared_values,
            installed: InstalledDomainKeys::default(),
            undo_published: BTreeSet::new(),
            durable_publications,
            durable_publication_markers: Vec::new(),
            durable_publications_installed: BTreeSet::new(),
            root_delta,
            persistent_gc_delta,
            volatile_objects,
            final_marker: None,
            lp_published: false,
            decision: MvccTerminalDecision::Abortable,
            root_applied: false,
            version_bumps_applied: false,
            completion_error: None,
            #[cfg(test)]
            inside_install_hook_ran: false,
            #[cfg(test)]
            before_publish_hook_ran: false,
            #[cfg(test)]
            after_publish_hook_ran: false,
        })?;
    drive_mvcc_terminal_commit(store)
}

#[cfg(feature = "transaction-mvcc")]
fn drive_mvcc_terminal_commit(store: &mut dyn VMStore) -> Result<()> {
    let mut terminal = store
        .store_opaque_mut()
        .transaction_state_mut()
        .take_mvcc_terminal_commit()
        .context("MVCC terminal commit state is missing")?;
    let visibility = store
        .store_opaque_mut()
        .transaction_state_mut()
        .active_mvcc_commit_context()?
        .0;

    if terminal.decision == MvccTerminalDecision::Rollback {
        let original = terminal
            .completion_error
            .clone()
            .unwrap_or_else(|| "MVCC commit rollback retry".to_string());
        return finish_mvcc_pre_decision_failure(store, &visibility, terminal, original);
    }

    let attempt = (|| -> Result<()> {
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterPreparation)?;
        install_mvcc_abortable_current_values(store, &visibility, &mut terminal)?;
        publish_mvcc_durable_values(store, &mut terminal)?;
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterDurablePublication)?;

        if let Some(marker) = terminal.final_marker {
            if store
                .store_opaque_mut()
                .transaction_state_mut()
                .take_fail_next_commit_before_lp_for_test()
            {
                bail!("transaction test failure before commit LP");
            }
            store
                .store_opaque_mut()
                .transaction_state_mut()
                .publish_commit_lp(terminal.stream_id, terminal.txid, marker)?;
            terminal.lp_published = true;
            terminal.decision = MvccTerminalDecision::ForceCommit;
            #[cfg(test)]
            visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterDurableLp)?;
        } else if mvcc_has_irreversible_current_work(&terminal) {
            // Markerless commits have no durable LP. Crossing this internal
            // decision keeps the record pending and the certification permit
            // held while synchronous force-completion installs values that
            // cannot be rolled back (notably monotonic size growth).
            terminal.decision = MvccTerminalDecision::ForceCommit;
        }

        if terminal.decision == MvccTerminalDecision::ForceCommit {
            force_complete_mvcc_commit(store, &visibility, &mut terminal)?;
        } else {
            publish_mvcc_record_and_cleanup(store, &visibility, &mut terminal)?;
        }
        Ok(())
    })();

    match attempt {
        Ok(()) => Ok(()),
        Err(error) if terminal.decision == MvccTerminalDecision::Abortable => {
            finish_mvcc_pre_decision_failure(store, &visibility, terminal, format!("{error:#}"))
        }
        Err(error) => {
            let original = format!("{error:#}");
            terminal.completion_error = Some(original.clone());
            let outcome = if terminal.lp_published {
                "committed durably"
            } else {
                "committed irrevocably"
            };
            if store
                .store_opaque_mut()
                .transaction_state_mut()
                .active_transaction()
                .is_none()
            {
                return Err(crate::format_err!(
                    "transaction {outcome} but completion/cleanup failed: {original}"
                ));
            }
            match force_complete_mvcc_commit(store, &visibility, &mut terminal) {
                Ok(()) => Err(crate::format_err!(
                    "transaction {outcome} but completion/cleanup failed: {original}"
                )),
                Err(completion_error) => {
                    let combined = crate::format_err!(
                        "transaction {outcome} but completion/cleanup failed: {original}; force-completion also failed: {completion_error:#}"
                    );
                    if store
                        .store_opaque_mut()
                        .transaction_state_mut()
                        .active_transaction()
                        .is_some()
                    {
                        store
                            .store_opaque_mut()
                            .transaction_state_mut()
                            .install_mvcc_terminal_commit(terminal)?;
                    }
                    Err(combined)
                }
            }
        }
    }
}

#[cfg(feature = "transaction-mvcc")]
fn finish_mvcc_pre_decision_failure(
    store: &mut dyn VMStore,
    visibility: &Arc<MvccRuntime>,
    mut terminal: MvccTerminalCommitState,
    original: String,
) -> Result<()> {
    terminal.decision = MvccTerminalDecision::Rollback;
    terminal.completion_error = Some(original.clone());
    let abort_record = match terminal.pending.as_mut() {
        Some(pending) => pending.abort(),
        None => Ok(()),
    };
    if abort_record.is_ok() {
        terminal.pending = None;
    }
    let rollback = rollback_mvcc_current_values(store, visibility, &mut terminal);
    let remove_versions = if abort_record.is_ok() && rollback.is_ok() {
        visibility.remove_aborted_commit_versions(&terminal.commit)
    } else {
        Ok(())
    };
    let typed_retry_required =
        abort_record.is_err() || rollback.is_err() || remove_versions.is_err();
    let cleanup = if !typed_retry_required {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.abort_allocated_objects(object_table)
    } else {
        Ok(())
    };

    let rollback_result = combine_operation_and_cleanup_results(
        abort_record,
        rollback,
        "failed to restore MVCC current values",
    );
    let rollback_result = combine_operation_and_cleanup_results(
        rollback_result,
        remove_versions,
        "failed to remove aborted MVCC versions",
    );
    let rollback_result = combine_operation_and_cleanup_results(
        rollback_result,
        cleanup,
        "failed to clean up aborted MVCC transaction",
    );
    match rollback_result {
        Ok(()) => Err(crate::format_err!("{original}")),
        Err(rollback_error) => {
            let active = store
                .store_opaque_mut()
                .transaction_state_mut()
                .active_transaction()
                .is_some();
            if typed_retry_required && active {
                store
                    .store_opaque_mut()
                    .transaction_state_mut()
                    .install_mvcc_terminal_commit(terminal)?;
                Err(crate::format_err!(
                    "{original}; MVCC rollback requires an explicit retry: {rollback_error:#}"
                ))
            } else {
                Err(crate::format_err!(
                    "{original}; MVCC rollback/cleanup also failed: {rollback_error:#}"
                ))
            }
        }
    }
}

#[cfg(feature = "transaction-mvcc")]
fn install_mvcc_abortable_current_values(
    store: &mut dyn VMStore,
    visibility: &Arc<MvccRuntime>,
    terminal: &mut MvccTerminalCommitState,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = visibility;

    let memories = terminal
        .prepared
        .memories
        .iter()
        .map(|(&key, value)| (key, value.clone()))
        .collect::<Vec<_>>();
    for (granule, prepared) in memories {
        if terminal.installed.memories.contains(&granule) {
            continue;
        }
        let requires_growth = mvcc_memory_install_requires_growth(terminal, granule)?;
        if requires_growth {
            if mvcc_memory_backend(store, terminal.instance, granule)? == TMemoryBackend::VMemory {
                continue;
            }
            reserve_mvcc_memory_capacity(store, terminal, granule)?;
            publish_mvcc_memory_undo_if_needed(store, terminal, granule, &prepared.value, true)?;
            install_mvcc_memory_value_within_capacity(
                store,
                terminal.instance,
                granule,
                &prepared.value,
            )?;
        } else {
            publish_mvcc_memory_undo_if_needed(store, terminal, granule, &prepared.value, false)?;
            install_mvcc_memory_value(store, terminal.instance, granule, &prepared.value)?;
        }
        terminal.installed.memories.insert(granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterMemoryInstall)?;
        #[cfg(test)]
        if !terminal.inside_install_hook_ran {
            terminal.inside_install_hook_ran = true;
            visibility.run_inside_install_hook_for_test()?;
        }
    }

    let globals = terminal
        .prepared
        .globals
        .iter()
        .map(|(&key, value)| (key, value.value))
        .collect::<Vec<_>>();
    for (granule, value) in globals {
        if terminal.installed.globals.contains(&granule) {
            continue;
        }
        install_mvcc_global_value(store, terminal.instance, granule, value)?;
        terminal.installed.globals.insert(granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterGlobalInstall)?;
    }

    let tables = terminal
        .prepared
        .tables
        .iter()
        .map(|(&key, value)| (key, value.clone()))
        .collect::<Vec<_>>();
    for (granule, prepared) in tables {
        if terminal.installed.tables.contains(&granule)
            || prepared.predecessor.elements().len() != prepared.value.elements().len()
        {
            continue;
        }
        install_mvcc_table_value(store, terminal.instance, granule, &prepared.value)?;
        terminal.installed.tables.insert(granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterTableInstall)?;
    }
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn publish_mvcc_memory_undo_if_needed(
    store: &mut dyn VMStore,
    terminal: &mut MvccTerminalCommitState,
    granule: GranuleId,
    value: &[u8],
    within_capacity: bool,
) -> Result<()> {
    if terminal.undo_published.contains(&granule) {
        return Ok(());
    }
    let GranuleId::TMemory {
        instance,
        memory_index,
        granule_index,
    } = granule
    else {
        bail!("MVCC memory installer received a non-memory granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(terminal.instance);
    let memory = TMemoryIndex::from_u32(memory_index);
    let undo = {
        let instance_ref = store.instance_mut(owner);
        let instance_ref = instance_ref.as_ref();
        let tmemory = instance_ref
            .get_tmemory(memory)
            .context("transactional memory operation targeted non-transactional memory")?;
        if tmemory.backend() == TMemoryBackend::VMemory {
            terminal.undo_published.insert(granule);
            return Ok(());
        }
        if within_capacity {
            tmemory.prepare_tmemory_undo_record_within_capacity_for_mvcc(
                instance,
                memory_index,
                granule_index,
                value,
            )?
        } else {
            tmemory.prepare_tmemory_undo_record(instance, memory_index, granule_index, value)?
        }
    };
    let marker = store
        .store_opaque_mut()
        .transaction_state_mut()
        .publish_tmemory_undo_before_in_place_write(terminal.stream_id, terminal.txid, &undo)?;
    terminal.final_marker = Some(marker);
    terminal.undo_published.insert(granule);
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn publish_mvcc_durable_values(
    store: &mut dyn VMStore,
    terminal: &mut MvccTerminalCommitState,
) -> Result<()> {
    while terminal.durable_publication_markers.len() < terminal.durable_publications.len() {
        let index = terminal.durable_publication_markers.len();
        let publication = terminal.durable_publications[index].clone();
        let markers = {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state.publish_object_publications_before_commit_with_markers(
                terminal.stream_id,
                terminal.txid,
                &*object_table,
                core::slice::from_ref(&publication),
            )?
        };
        let marker = *markers
            .first()
            .context("durable MVCC publication produced no marker")?;
        terminal.durable_publication_markers.push(marker);
        terminal.final_marker = Some(marker);
    }
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn mvcc_has_irreversible_current_work(terminal: &MvccTerminalCommitState) -> bool {
    !terminal.prepared.memory_sizes.is_empty()
        || !terminal.prepared.table_sizes.is_empty()
        || !terminal.volatile_objects.is_empty()
        || terminal
            .prepared
            .memories
            .keys()
            .any(|key| !terminal.installed.memories.contains(key))
        || terminal
            .prepared
            .tables
            .keys()
            .any(|key| !terminal.installed.tables.contains(key))
}

#[cfg(feature = "transaction-mvcc")]
fn force_complete_mvcc_commit(
    store: &mut dyn VMStore,
    visibility: &Arc<MvccRuntime>,
    terminal: &mut MvccTerminalCommitState,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = visibility;

    terminal.decision = MvccTerminalDecision::ForceCommit;

    let memory_sizes = terminal
        .prepared
        .memory_sizes
        .iter()
        .map(|(&key, value)| (key, value.value))
        .collect::<Vec<_>>();
    for (granule, value) in memory_sizes {
        if terminal.installed.memory_sizes.contains(&granule) {
            continue;
        }
        let GranuleId::TMemorySize {
            instance,
            memory_index,
        } = granule
        else {
            bail!("MVCC memory-size installer received a non-size granule");
        };
        grow_tmemory_to_pages(
            store,
            instance
                .map(InstanceId::from_u32)
                .unwrap_or(terminal.instance),
            memory_index,
            value,
        )?;
        terminal.installed.memory_sizes.insert(granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterMemorySizeInstall)?;
    }

    let table_sizes = terminal
        .prepared
        .table_sizes
        .iter()
        .map(|(&key, value)| (key, value.value))
        .collect::<Vec<_>>();
    for (granule, value) in table_sizes {
        if terminal.installed.table_sizes.contains(&granule) {
            continue;
        }
        let GranuleId::TTableSize {
            instance,
            table_index,
        } = granule
        else {
            bail!("MVCC table-size installer received a non-size granule");
        };
        grow_defined_table_to(
            store,
            instance
                .map(InstanceId::from_u32)
                .unwrap_or(terminal.instance),
            table_index,
            value,
        )?;
        terminal.installed.table_sizes.insert(granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterTableSizeInstall)?;
    }

    let memories = terminal
        .prepared
        .memories
        .iter()
        .map(|(&key, value)| (key, value.value.clone()))
        .collect::<Vec<_>>();
    for (granule, value) in memories {
        if terminal.installed.memories.contains(&granule) {
            continue;
        }
        install_mvcc_memory_value(store, terminal.instance, granule, &value)?;
        terminal.installed.memories.insert(granule);
    }
    let globals = terminal
        .prepared
        .globals
        .iter()
        .map(|(&key, value)| (key, value.value))
        .collect::<Vec<_>>();
    for (granule, value) in globals {
        if terminal.installed.globals.contains(&granule) {
            continue;
        }
        install_mvcc_global_value(store, terminal.instance, granule, value)?;
        terminal.installed.globals.insert(granule);
    }
    let tables = terminal
        .prepared
        .tables
        .iter()
        .map(|(&key, value)| (key, value.value.clone()))
        .collect::<Vec<_>>();
    for (granule, value) in tables {
        if terminal.installed.tables.contains(&granule) {
            continue;
        }
        install_mvcc_table_value(store, terminal.instance, granule, &value)?;
        terminal.installed.tables.insert(granule);
    }

    let volatile = terminal.volatile_objects.clone();
    for (object, payload) in volatile {
        if terminal.installed.objects.contains(&object) {
            continue;
        }
        store
            .store_opaque_mut()
            .transaction_object_table_mut()
            .install_current_payload_snapshot(object, &payload)?;
        terminal.installed.objects.insert(object);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterObjectInstall)?;
    }

    #[cfg(test)]
    visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::BeforeMappedObjectInstall)?;
    for (index, publication) in terminal.durable_publications.iter().enumerate() {
        if terminal.durable_publications_installed.contains(&index) {
            continue;
        }
        let marker = terminal
            .durable_publication_markers
            .get(index)
            .copied()
            .context("durable MVCC publication marker is missing")?;
        let installed = {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state.install_committed_mapped_object_publications(
                object_table,
                core::slice::from_ref(publication),
                core::slice::from_ref(&marker),
            )?
        };
        terminal.installed.objects.extend(installed);
        terminal.durable_publications_installed.insert(index);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterObjectInstall)?;
    }

    if !terminal.root_applied {
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRootApply)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .prepare_mvcc_root_commit_completion(terminal.root_delta.clone())?;
        terminal.root_applied = true;
    }
    if !terminal.version_bumps_applied {
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .prepare_mvcc_version_commit_completion()?;
        terminal.version_bumps_applied = true;
    }

    publish_mvcc_record_and_cleanup(store, visibility, terminal)
}

#[cfg(feature = "transaction-mvcc")]
fn publish_mvcc_record_and_cleanup(
    store: &mut dyn VMStore,
    visibility: &Arc<MvccRuntime>,
    terminal: &mut MvccTerminalCommitState,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = visibility;

    #[cfg(test)]
    if !terminal.before_publish_hook_ran {
        terminal.before_publish_hook_ran = true;
        visibility.run_before_publish_hook_for_test()?;
    }
    if let Some(pending) = terminal.pending.as_mut() {
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::BeforeRecordPublication)?;
        let _timestamp = pending.publish()?;
        if terminal.final_marker.is_none() {
            terminal.decision = MvccTerminalDecision::ForceCommit;
        }
        terminal.pending = None;
    }
    #[cfg(test)]
    visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::AfterRecordTransition)?;
    #[cfg(test)]
    if !terminal.after_publish_hook_ran {
        terminal.after_publish_hook_ran = true;
        visibility.run_after_publish_hook_for_test()?;
    }
    let persistent_gc_delta = terminal.persistent_gc_delta.clone();
    {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let _ = state.observe_persistent_gc_commit_delta_after_commit_best_effort(
            object_table,
            &persistent_gc_delta,
        );
    }
    #[cfg(test)]
    visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringCleanup)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .finish_mvcc_commit_cleanup()?;
    terminal.certification.take();
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn rollback_mvcc_current_values(
    store: &mut dyn VMStore,
    visibility: &Arc<MvccRuntime>,
    terminal: &mut MvccTerminalCommitState,
) -> Result<()> {
    #[cfg(not(test))]
    let _ = visibility;

    let objects = terminal
        .installed
        .objects
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    for object in objects {
        let prepared = terminal
            .prepared
            .objects
            .get(&object)
            .context("installed MVCC object has no prepared value")?
            .clone();
        let object_table = store.store_opaque_mut().transaction_object_table_mut();
        match prepared.predecessor {
            Some(predecessor) => {
                object_table.install_current_payload_snapshot(object, &predecessor)?
            }
            None => {
                object_table.free(object)?;
            }
        }
        terminal.installed.objects.remove(&object);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRollback)?;
    }

    ensure!(
        terminal.installed.table_sizes.is_empty(),
        "MVCC rollback cannot shrink an installed table size"
    );
    let tables = terminal
        .installed
        .tables
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    for granule in tables {
        let predecessor = terminal
            .prepared
            .tables
            .get(&granule)
            .context("installed MVCC table has no predecessor")?
            .predecessor
            .clone();
        install_mvcc_table_value(store, terminal.instance, granule, &predecessor)?;
        terminal.installed.tables.remove(&granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRollback)?;
    }

    let globals = terminal
        .installed
        .globals
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    for granule in globals {
        let predecessor = terminal
            .prepared
            .globals
            .get(&granule)
            .context("installed MVCC global has no predecessor")?
            .predecessor;
        install_mvcc_global_value(store, terminal.instance, granule, predecessor)?;
        terminal.installed.globals.remove(&granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRollback)?;
    }

    ensure!(
        terminal.installed.memory_sizes.is_empty(),
        "MVCC rollback cannot shrink an installed memory size"
    );
    let memories = terminal
        .installed
        .memories
        .iter()
        .rev()
        .copied()
        .collect::<Vec<_>>();
    for granule in memories {
        let predecessor = terminal
            .prepared
            .memories
            .get(&granule)
            .context("installed MVCC memory has no predecessor")?
            .predecessor
            .clone();
        if mvcc_memory_install_requires_growth(terminal, granule)? {
            install_mvcc_memory_value_within_capacity(
                store,
                terminal.instance,
                granule,
                &predecessor,
            )?;
        } else {
            install_mvcc_memory_value(store, terminal.instance, granule, &predecessor)?;
        }
        terminal.installed.memories.remove(&granule);
        #[cfg(test)]
        visibility.inject_commit_fault_for_test(MvccCommitFaultPoint::DuringRollback)?;
    }
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn mvcc_memory_install_requires_growth(
    terminal: &MvccTerminalCommitState,
    granule: GranuleId,
) -> Result<bool> {
    let GranuleId::TMemory {
        instance,
        memory_index,
        granule_index,
    } = granule
    else {
        bail!("MVCC memory growth check received a non-memory granule");
    };
    let size_key = GranuleId::TMemorySize {
        instance,
        memory_index,
    };
    let Some(size) = terminal.prepared.memory_sizes.get(&size_key) else {
        return Ok(false);
    };
    let granule_start = granule_index
        .checked_mul(u64::try_from(TMEMORY_GRANULE_SIZE).unwrap())
        .context("MVCC memory granule offset overflow")?;
    let old_bytes = size
        .predecessor
        .checked_mul(u64::try_from(crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE).unwrap())
        .context("MVCC predecessor memory size overflow")?;
    Ok(granule_start >= old_bytes)
}

#[cfg(feature = "transaction-mvcc")]
fn mvcc_memory_backend(
    store: &mut dyn VMStore,
    default_instance: InstanceId,
    granule: GranuleId,
) -> Result<TMemoryBackend> {
    let GranuleId::TMemory {
        instance,
        memory_index,
        ..
    } = granule
    else {
        bail!("MVCC memory backend query received a non-memory granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(default_instance);
    let instance_ref = store.instance_mut(owner);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(TMemoryIndex::from_u32(memory_index))
        .context("transactional memory operation targeted non-transactional memory")?;
    Ok(tmemory.backend())
}

#[cfg(feature = "transaction-mvcc")]
fn reserve_mvcc_memory_capacity(
    store: &mut dyn VMStore,
    terminal: &MvccTerminalCommitState,
    granule: GranuleId,
) -> Result<()> {
    let GranuleId::TMemory {
        instance,
        memory_index,
        ..
    } = granule
    else {
        bail!("MVCC memory capacity reservation received a non-memory granule");
    };
    let size_key = GranuleId::TMemorySize {
        instance,
        memory_index,
    };
    let target_pages = terminal
        .prepared
        .memory_sizes
        .get(&size_key)
        .context("MVCC hidden-capacity install is missing its prepared memory size")?
        .value;
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(terminal.instance);
    let mut instance_ref = store.instance_mut(owner);
    let tmemory = instance_ref
        .as_mut()
        .get_tmemory_mut(TMemoryIndex::from_u32(memory_index))
        .context("transactional memory operation targeted non-transactional memory")?;
    tmemory.reserve_backing_capacity_to_pages_for_mvcc(target_pages)
}

#[cfg(feature = "transaction-mvcc")]
fn install_mvcc_memory_value(
    store: &mut dyn VMStore,
    default_instance: InstanceId,
    granule: GranuleId,
    value: &[u8],
) -> Result<()> {
    let GranuleId::TMemory {
        instance,
        memory_index,
        granule_index,
    } = granule
    else {
        bail!("MVCC memory installer received a non-memory granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(default_instance);
    let mut instance_ref = store.instance_mut(owner);
    let tmemory = instance_ref
        .as_mut()
        .get_tmemory_mut(TMemoryIndex::from_u32(memory_index))
        .context("transactional memory operation targeted non-transactional memory")?;
    tmemory.commit_staged_tmemory_granules_direct(&[(granule_index, value.to_vec())])
}

#[cfg(feature = "transaction-mvcc")]
fn install_mvcc_memory_value_within_capacity(
    store: &mut dyn VMStore,
    default_instance: InstanceId,
    granule: GranuleId,
    value: &[u8],
) -> Result<()> {
    let GranuleId::TMemory {
        instance,
        memory_index,
        granule_index,
    } = granule
    else {
        bail!("MVCC memory capacity installer received a non-memory granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(default_instance);
    let mut instance_ref = store.instance_mut(owner);
    let tmemory = instance_ref
        .as_mut()
        .get_tmemory_mut(TMemoryIndex::from_u32(memory_index))
        .context("transactional memory operation targeted non-transactional memory")?;
    tmemory.commit_staged_tmemory_granule_within_capacity_for_mvcc(granule_index, value)
}

#[cfg(feature = "transaction-mvcc")]
fn install_mvcc_global_value(
    store: &mut dyn VMStore,
    default_instance: InstanceId,
    granule: GranuleId,
    value: GlobalSnapshot,
) -> Result<()> {
    let GranuleId::TGlobal {
        instance,
        global_index,
    } = granule
    else {
        bail!("MVCC global installer received a non-global granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(default_instance);
    let global_index = TGlobalIndex::from_u32(global_index);
    let (_, _, wasm_ty) = transaction_global(store, owner, global_index.as_u32())?;
    let mut global = global_definition_ptr(store, owner, global_index)?;
    write_transaction_global_snapshot(
        store.store_opaque_mut(),
        unsafe { global.as_mut() },
        wasm_ty,
        value,
    )
}

#[cfg(feature = "transaction-mvcc")]
fn install_mvcc_table_value(
    store: &mut dyn VMStore,
    default_instance: InstanceId,
    granule: GranuleId,
    value: &TableGranuleSnapshot,
) -> Result<()> {
    let GranuleId::TTable {
        instance,
        table_index,
        granule_index,
    } = granule
    else {
        bail!("MVCC table installer received a non-table granule");
    };
    let owner = instance
        .map(InstanceId::from_u32)
        .unwrap_or(default_instance);
    let start = granule_index
        .checked_mul(TableGranuleSnapshot::ELEMENT_CAPACITY)
        .context("MVCC table granule offset overflow")?;
    for (offset, element) in value.elements().iter().copied().enumerate() {
        let element_index = start
            .checked_add(u64::try_from(offset).context("MVCC table offset overflow")?)
            .context("MVCC table element index overflow")?;
        write_table_element_snapshot(store, owner, table_index, element_index, element)?;
    }
    Ok(())
}

#[cfg(feature = "transaction-mvcc")]
fn current_tmemory_granule(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: u32,
    granule_index: u64,
    value_len: usize,
) -> Result<Vec<u8>> {
    let addr = granule_index
        .checked_mul(u64::try_from(crate::runtime::transaction::TMEMORY_GRANULE_SIZE).unwrap())
        .context("tmemory granule address overflow")?;
    let memory_index = TMemoryIndex::from_u32(memory_index);
    let pages = current_tmemory_pages(store, instance, memory_index)?;
    let byte_len = pages
        .checked_mul(u64::try_from(crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE).unwrap())
        .context("tmemory byte length overflow")?;
    if addr >= byte_len {
        return Ok(vec![0; value_len]);
    }
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(memory_index)
        .context("transactional memory operation targeted non-transactional memory")?;
    Ok(collect_tmemory_access_snapshot(tmemory, addr, value_len)?
        .into_granules()
        .into_iter()
        .next()
        .context("current tmemory snapshot is missing its granule")?
        .into_bytes())
}

#[cfg(feature = "transaction-mvcc")]
fn read_global_snapshot_like(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: TGlobalIndex,
    value: GlobalSnapshot,
) -> Result<GlobalSnapshot> {
    let (_, _, wasm_ty) = transaction_global(store, instance, global.as_u32())?;
    let global = global_definition_ptr(store, instance, global)?;
    let global = unsafe { global.as_ref() };
    Ok(match value {
        GlobalSnapshot::I32(_) => GlobalSnapshot::I32(unsafe { *global.as_i32() }),
        GlobalSnapshot::I64(_) => GlobalSnapshot::I64(unsafe { *global.as_i64() }),
        GlobalSnapshot::F32(_) => GlobalSnapshot::F32(unsafe { *global.as_f32_bits() }),
        GlobalSnapshot::F64(_) => GlobalSnapshot::F64(unsafe { *global.as_f64_bits() }),
        GlobalSnapshot::V128(_) => GlobalSnapshot::V128(unsafe { *global.as_u128_bits() }),
        GlobalSnapshot::FuncRef(_) => {
            GlobalSnapshot::FuncRef(unsafe { global.as_func_ref() as usize })
        }
        GlobalSnapshot::GcRef(_) => {
            let raw = match wasm_ty {
                WasmValType::Ref(ref_ty)
                    if ref_ty.is_transactional_ref()
                        && ref_ty.heap_type.top() == WasmHeapTopType::Extern =>
                unsafe { global.as_gc_ref().map_or(0, VMGcRef::as_raw_u32) },
                WasmValType::Ref(ref_ty) if ref_ty.is_transactional_ref() => unsafe {
                    *global.as_u32()
                },
                _ => unsafe { global.as_gc_ref().map_or(0, VMGcRef::as_raw_u32) },
            };
            match wasm_ty {
                WasmValType::Ref(ref_ty)
                    if ref_ty.is_transactional_ref()
                        && ref_ty.heap_type.top() == WasmHeapTopType::Extern =>
                {
                    transaction_externalized_global_snapshot(store.store_opaque(), raw)?
                }
                _ => GlobalSnapshot::GcRef(raw),
            }
        }
        GlobalSnapshot::ExternalizedRef { .. } => {
            #[cfg(not(all(feature = "gc", feature = "transaction")))]
            bail!("transactional external conversion requires GC support");
            #[cfg(all(feature = "gc", feature = "transaction"))]
            {
                let wrapper = unsafe { *global.as_u32() };
                let inner = transaction_externalized_ref_raw(store.store_opaque(), wrapper)?
                    .context("transactional external wrapper lost its native identity")?;
                GlobalSnapshot::ExternalizedRef { wrapper, inner }
            }
        }
    })
}

fn transaction_fail(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    state.fail_allocated_objects(object_table)
}

fn transaction_fail_with_code(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    code: u32,
) -> Result<()> {
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    state.fail_allocated_objects_with_code(object_table, code)
}

fn transaction_failure_pending(store: &mut dyn VMStore, _instance: InstanceId) -> u32 {
    u32::from(
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .structured_failure_pending(),
    )
}

fn transaction_failure_code(store: &mut dyn VMStore, _instance: InstanceId) -> u32 {
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .structured_failure_code()
}

const TRANSACTION_TREF_TEST_KIND_EQ: u32 = 1;
const TRANSACTION_TREF_TEST_KIND_STRUCT: u32 = 3;
const TRANSACTION_TREF_TEST_KIND_ARRAY: u32 = 4;
const TRANSACTION_TREF_TEST_EXPECTED_TYPE_NONE: u32 = u32::MAX;

#[cfg(feature = "gc")]
fn transaction_module_type_index_to_shared(
    store: &mut dyn VMStore,
    instance: InstanceId,
    type_index: u32,
) -> Result<VMSharedTypeIndex> {
    let instance = store.instance(instance);
    let module_type = TypeIndex::from_u32(type_index);
    match instance.env_module().types[module_type] {
        wasmtime_environ::EngineOrModuleTypeIndex::Engine(engine_type) => Ok(engine_type),
        wasmtime_environ::EngineOrModuleTypeIndex::Module(module_type) => {
            Ok(instance.engine_type_index(module_type))
        }
        wasmtime_environ::EngineOrModuleTypeIndex::RecGroup(_) => {
            bail!("transaction object constructor received a recgroup-relative type index")
        }
    }
}

#[cfg(not(feature = "gc"))]
fn transaction_module_type_index_to_shared(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    _type_index: u32,
) -> Result<VMSharedTypeIndex> {
    bail!("transaction object constructors require GC support")
}

fn transaction_tref_test(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    raw_ref: u32,
    test_kind: u32,
    nullable: u32,
    expected_engine_type: u32,
) -> u32 {
    if raw_ref == 0 {
        return u32::from(nullable != 0);
    }

    if ObjectTable::is_raw_i31_ref(u64::from(raw_ref)) {
        return u32::from(test_kind == TRANSACTION_TREF_TEST_KIND_EQ);
    }

    let store = store.store_opaque();
    let state = store.transaction_state();
    let object_table = store.transaction_object_table();
    let Some(object_id) = state.known_object_id_for_transaction_ref_handle(object_table, raw_ref)
    else {
        // Non-object identities in the transaction-any hierarchy are
        // converted transactional extern references. They cannot satisfy an
        // eq/struct/array test, and must never be passed to the ordinary GC
        // reference machinery.
        return 0;
    };
    let Ok(kind) = state.object_kind(object_table, object_id) else {
        return 0;
    };

    let abstract_match = match test_kind {
        TRANSACTION_TREF_TEST_KIND_EQ => {
            matches!(
                kind,
                ObjectKind::Struct | ObjectKind::Array | ObjectKind::I31
            )
        }
        TRANSACTION_TREF_TEST_KIND_STRUCT => kind == ObjectKind::Struct,
        TRANSACTION_TREF_TEST_KIND_ARRAY => kind == ObjectKind::Array,
        _ => return 0,
    };
    if !abstract_match {
        return 0;
    }

    if expected_engine_type == TRANSACTION_TREF_TEST_EXPECTED_TYPE_NONE {
        return 1;
    }

    let Ok(Some(actual)) = state.runtime_type_index(object_table, object_id) else {
        return 0;
    };
    let expected = VMSharedTypeIndex::from_u32(expected_engine_type);
    u32::from(store.engine().signatures().is_subtype(actual, expected))
}

fn transaction_tref_cast_read(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    let result = transaction_tref_cast_read_impl(store, instance, ref_handle);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tref_cast_read_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    state.acquire_tref_read_for_transaction_ref_handle(object_table, ref_handle)?;
    Ok(())
}

fn transaction_tref_cast_write(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    let result = transaction_tref_cast_write_impl(store, instance, ref_handle);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tref_cast_write_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    state.acquire_tref_write_for_transaction_ref_handle(object_table, ref_handle)?;
    Ok(())
}

fn transaction_tglobal_get(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
) -> Result<*mut u8> {
    let result = transaction_tglobal_get_impl(store, instance, global);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    tag: u32,
    value: u64,
) -> Result<()> {
    let result = transaction_tglobal_set_impl(store, instance, global, tag, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_set_v128(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_tglobal_set_v128_impl(store, instance, global, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_get_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;

    let (owner, global_index, wasm_ty) = transaction_global(store, instance, global)?;
    if store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_none()
    {
        let snapshot = read_global_snapshot(store, owner, global_index, wasm_ty)?;
        ensure_global_snapshot_type(snapshot, wasm_ty)?;
        let snapshot = live_transaction_global_snapshot_for_read(store, snapshot, wasm_ty)?;
        let bytes = global_snapshot_bytes(snapshot);
        return Ok(store
            .store_opaque_mut()
            .transaction_state_mut()
            .set_scratch(bytes));
    }

    let (staged, visibility) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        ensure!(
            state.active_transaction().is_some(),
            "transaction operation requires an active transaction"
        );
        state.acquire_global_read_owned(Some(owner), global_index.as_u32())?;
        (
            state.staged_global_owned(Some(owner), global_index.as_u32()),
            state.active_visibility_read_context()?,
        )
    };
    let snapshot = match staged {
        Some(snapshot) => snapshot,
        None => visibility.read_global(
            GranuleId::TGlobal {
                instance: Some(owner.as_u32()),
                global_index: global_index.as_u32(),
            },
            || read_global_snapshot(store, owner, global_index, wasm_ty),
        )?,
    };
    ensure_global_snapshot_type(snapshot, wasm_ty)?;

    let snapshot = live_transaction_global_snapshot_for_read(store, snapshot, wasm_ty)?;
    let bytes = global_snapshot_bytes(snapshot);
    Ok(store
        .store_opaque_mut()
        .transaction_state_mut()
        .set_scratch(bytes))
}

fn transaction_tglobal_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    tag: u32,
    value: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    let (owner, global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let snapshot = global_snapshot_from_tag(tag, value)?;
    ensure_active_transaction(store, instance)?;
    stage_transaction_global_snapshot(store, owner, global_index, wasm_ty, snapshot)
}

fn transaction_tglobal_startup_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    tag: u32,
    value: u64,
) -> Result<()> {
    let result = transaction_tglobal_startup_set_impl(store, instance, global, tag, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_startup_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    tag: u32,
    value: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    let (owner, global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let snapshot = global_snapshot_from_tag(tag, value)?;
    write_or_stage_transaction_global_snapshot(store, owner, global_index, wasm_ty, snapshot)
}

fn stage_transaction_global_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global_index: TGlobalIndex,
    wasm_ty: WasmValType,
    snapshot: GlobalSnapshot,
) -> Result<()> {
    ensure_global_snapshot_type(snapshot, wasm_ty)?;
    #[cfg(feature = "transaction")]
    if let (GlobalSnapshot::GcRef(wrapper), WasmValType::Ref(ref_ty)) = (snapshot, wasm_ty)
        && ref_ty.is_transactional_ref()
        && ref_ty.heap_type.top() == WasmHeapTopType::Extern
    {
        let snapshot = if wrapper == 0 {
            GlobalSnapshot::GcRef(0)
        } else {
            let rooted = {
                let mut no_gc = AutoAssertNoGc::new(store.store_opaque_mut());
                ExternRef::_from_raw(&mut no_gc, wrapper)
                    .context("transactional external reference is null")?
            };
            let _ = store.store_opaque_mut().transaction_extern_handle(rooted)?;
            match transaction_externalized_ref_raw(store.store_opaque(), wrapper)? {
                Some(inner) => {
                    transaction_preserve_tref_result_impl(store.store_opaque_mut(), inner)?;
                    GlobalSnapshot::ExternalizedRef { wrapper, inner }
                }
                None => GlobalSnapshot::GcRef(wrapper),
            }
        };
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .stage_global_owned(Some(instance), global_index.as_u32(), snapshot)?;
        return Ok(());
    }
    if let GlobalSnapshot::GcRef(gc_ref) = snapshot {
        let abi = ObjectValueAbi::from_live_parts(
            OBJECT_VALUE_ABI_TAG_REF,
            u64::from(gc_ref),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
        )?;
        let normalized = object_value_from_persistent_slot_abi(store.store_opaque_mut(), abi)?;
        let promoted_snapshot = {
            let store = store.store_opaque_mut();
            let (durable_refs, state, object_table) =
                store.transaction_durable_refs_state_and_object_table_mut();
            GlobalSnapshot::GcRef(gc_snapshot_raw_from_object_value(
                &*durable_refs,
                state,
                object_table,
                normalized,
                "transactional global GC reference",
            )?)
        };
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .stage_global_owned(Some(instance), global_index.as_u32(), promoted_snapshot)?;
        return Ok(());
    }
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_global_owned(Some(instance), global_index.as_u32(), snapshot)?;
    Ok(())
}

fn write_or_stage_transaction_global_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global_index: TGlobalIndex,
    wasm_ty: WasmValType,
    snapshot: GlobalSnapshot,
) -> Result<()> {
    if store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some()
    {
        return stage_transaction_global_snapshot(store, instance, global_index, wasm_ty, snapshot);
    }
    write_transaction_global_snapshot_direct(store, instance, global_index, wasm_ty, snapshot)
}

fn write_transaction_global_snapshot_direct(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global_index: TGlobalIndex,
    wasm_ty: WasmValType,
    snapshot: GlobalSnapshot,
) -> Result<()> {
    ensure_global_snapshot_type(snapshot, wasm_ty)?;
    let mut global = global_definition_ptr(store, instance, global_index)?;
    write_transaction_global_snapshot(
        store.store_opaque_mut(),
        unsafe { global.as_mut() },
        wasm_ty,
        snapshot,
    )
}

fn transaction_tglobal_set_v128_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    let (owner, global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let snapshot = GlobalSnapshot::V128(unsafe { *value.cast::<[u8; 16]>() });
    ensure_active_transaction(store, instance)?;
    stage_transaction_global_snapshot(store, owner, global_index, wasm_ty, snapshot)
}

fn transaction_tglobal_startup_set_v128(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_tglobal_startup_set_v128_impl(store, instance, global, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_startup_set_v128_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    let (owner, global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let snapshot = GlobalSnapshot::V128(unsafe { *value.cast::<[u8; 16]>() });
    write_or_stage_transaction_global_snapshot(store, owner, global_index, wasm_ty, snapshot)
}

fn transaction_global(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
) -> Result<(InstanceId, TGlobalIndex, WasmValType)> {
    let global = TGlobalIndex::from_u32(global);
    let import = {
        let instance_ref = store.instance_mut(instance);
        let instance_ref = instance_ref.as_ref();
        let module = instance_ref.env_module();
        if module.defined_tglobal_index(global).is_some() {
            return Ok((instance, global, module.tglobals[global].wasm_ty));
        }
        *instance_ref.imported_tglobal(global)
    };

    let vm::VMGlobalKind::Instance(physical_index) = import.kind else {
        bail!("transactional globals must be backed by a core Wasm instance")
    };
    let vmctx = import
        .vmctx
        .context("transactional global import is missing its owning VMContext")?;
    // SAFETY: validated instance imports contain the live VMContext of their
    // provider in the same store.
    let owner =
        unsafe { vm::Instance::vmctx_instance_id(vm::VMContext::from_opaque(vmctx.as_non_null())) };
    let owner_instance = store.instance(owner);
    let owner_module = owner_instance.env_module();
    let defined = owner_module
        .defined_tglobal_index_from_runtime(physical_index)
        .context("transactional global import refers to an ordinary global slot")?;
    let global = owner_module.tglobal_index(defined);
    Ok((owner, global, owner_module.tglobals[global].wasm_ty))
}

fn global_snapshot_from_tag(tag: u32, value: u64) -> Result<GlobalSnapshot> {
    match tag {
        0 => Ok(GlobalSnapshot::I32(value as u32 as i32)),
        1 => Ok(GlobalSnapshot::I64(value as i64)),
        2 => Ok(GlobalSnapshot::F32(value as u32)),
        3 => Ok(GlobalSnapshot::F64(value)),
        4 => Ok(GlobalSnapshot::GcRef(value as u32)),
        5 => Ok(GlobalSnapshot::FuncRef(usize::try_from(value)?)),
        _ => bail!("unknown transactional global value tag: {tag}"),
    }
}

fn live_transaction_global_snapshot_for_read(
    store: &mut dyn VMStore,
    snapshot: GlobalSnapshot,
    wasm_ty: WasmValType,
) -> Result<GlobalSnapshot> {
    if let WasmValType::Ref(ref_ty) = wasm_ty
        && ref_ty.is_transactional_ref()
        && ref_ty.heap_type.top() == WasmHeapTopType::Extern
    {
        return Ok(snapshot);
    }
    match snapshot {
        GlobalSnapshot::GcRef(gc_ref) => Ok(GlobalSnapshot::GcRef(
            live_transaction_gc_ref_for_read(store, gc_ref)?,
        )),
        other => Ok(other),
    }
}

fn transaction_externalized_global_snapshot(
    store: &StoreOpaque,
    wrapper: u32,
) -> Result<GlobalSnapshot> {
    if wrapper == 0 {
        return Ok(GlobalSnapshot::GcRef(0));
    }
    #[cfg(not(all(feature = "gc", feature = "transaction")))]
    {
        let _ = store;
        bail!("transactional external conversion requires GC support")
    }
    #[cfg(all(feature = "gc", feature = "transaction"))]
    {
        Ok(match transaction_externalized_ref_raw(store, wrapper)? {
            Some(inner) => GlobalSnapshot::ExternalizedRef { wrapper, inner },
            None => GlobalSnapshot::GcRef(wrapper),
        })
    }
}

fn live_transaction_gc_ref_for_read(store: &mut dyn VMStore, gc_ref: u32) -> Result<u32> {
    if gc_ref == 0 || ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
        return Ok(gc_ref);
    }

    let store = store.store_opaque_mut();
    let (engine, gc_store, durable_refs, state, object_table) =
        store.transaction_promotion_context_mut();
    if state
        .known_object_id_for_transaction_ref_handle(object_table, gc_ref)
        .is_some()
    {
        return Ok(gc_ref);
    }
    let promoted = {
        let mut adapter =
            StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state.promote_gc_ref_for_live_transaction_ref_with_adapter(
            object_table,
            gc_ref,
            &mut adapter,
        )?
    };
    let Some(object_id) = promoted else {
        return Ok(gc_ref);
    };
    state.transaction_ref_handle_for_object_id_avoiding(object_table, object_id, |raw| {
        live_ref_raw_is_registered(&*durable_refs, raw)
    })
}

fn read_global_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: TGlobalIndex,
    ty: WasmValType,
) -> Result<GlobalSnapshot> {
    let global = global_definition_ptr(store, instance, global)?;
    let global = unsafe { global.as_ref() };
    match ty {
        WasmValType::I32 => Ok(GlobalSnapshot::I32(unsafe { *global.as_i32() })),
        WasmValType::I64 => Ok(GlobalSnapshot::I64(unsafe { *global.as_i64() })),
        WasmValType::F32 => Ok(GlobalSnapshot::F32(unsafe { *global.as_f32_bits() })),
        WasmValType::F64 => Ok(GlobalSnapshot::F64(unsafe { *global.as_f64_bits() })),
        WasmValType::V128 => Ok(GlobalSnapshot::V128(unsafe { *global.as_u128_bits() })),
        WasmValType::Ref(ref_ty) => match ref_ty.heap_type.top() {
            WasmHeapTopType::Func => Ok(GlobalSnapshot::FuncRef(unsafe {
                global.as_func_ref() as usize
            })),
            WasmHeapTopType::Any | WasmHeapTopType::Extern | WasmHeapTopType::Exn => {
                let raw = if ref_ty.is_transactional_ref()
                    && ref_ty.heap_type.top() == WasmHeapTopType::Extern
                {
                    unsafe { global.as_gc_ref().map_or(0, VMGcRef::as_raw_u32) }
                } else if ref_ty.is_transactional_ref() {
                    unsafe { *global.as_u32() }
                } else {
                    unsafe { global.as_gc_ref().map_or(0, VMGcRef::as_raw_u32) }
                };
                if ref_ty.is_transactional_ref()
                    && ref_ty.heap_type.top() == WasmHeapTopType::Extern
                {
                    transaction_externalized_global_snapshot(store.store_opaque(), raw)
                } else {
                    Ok(GlobalSnapshot::GcRef(raw))
                }
            }
            WasmHeapTopType::Cont => bail!("transactional contref global is not implemented yet"),
        },
    }
}

fn global_definition_ptr(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: TGlobalIndex,
) -> Result<NonNull<vm::VMGlobalDefinition>> {
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let module = instance_ref.env_module();
    if let Some(defined) = module.defined_tglobal_index(global) {
        return Ok(instance_ref.global_ptr(module.runtime_defined_tglobal_index(defined)));
    }
    Ok(instance_ref.imported_tglobal(global).from.as_non_null())
}

fn write_global_snapshot(
    store: &mut StoreOpaque,
    global: &mut vm::VMGlobalDefinition,
    value: GlobalSnapshot,
) -> Result<()> {
    unsafe {
        match value {
            GlobalSnapshot::I32(value) => *global.as_i32_mut() = value,
            GlobalSnapshot::I64(value) => *global.as_i64_mut() = value,
            GlobalSnapshot::F32(value) => *global.as_f32_bits_mut() = value,
            GlobalSnapshot::F64(value) => *global.as_f64_bits_mut() = value,
            GlobalSnapshot::V128(value) => global.as_u128_bits_mut().copy_from_slice(&value),
            GlobalSnapshot::GcRef(value) => {
                let value = VMGcRef::from_raw_u32(value);
                global.write_gc_ref(store, value.as_ref())?;
            }
            GlobalSnapshot::ExternalizedRef { wrapper, .. } => {
                let value = VMGcRef::from_raw_u32(wrapper);
                global.write_gc_ref(store, value.as_ref())?;
            }
            GlobalSnapshot::FuncRef(value) => {
                *global.as_func_ref_mut() = core::ptr::with_exposed_provenance_mut(value);
            }
        }
    }
    Ok(())
}

fn write_transaction_global_snapshot(
    store: &mut StoreOpaque,
    global: &mut vm::VMGlobalDefinition,
    wasm_ty: WasmValType,
    value: GlobalSnapshot,
) -> Result<()> {
    if let WasmValType::Ref(ref_ty) = wasm_ty
        && ref_ty.is_transactional_ref()
    {
        let raw = match value {
            GlobalSnapshot::GcRef(value) => Some(value),
            GlobalSnapshot::ExternalizedRef { wrapper, .. } => Some(wrapper),
            _ => None,
        };
        if let Some(raw) = raw {
            if ref_ty.heap_type.top() == WasmHeapTopType::Extern {
                let value = VMGcRef::from_raw_u32(raw);
                unsafe {
                    global.write_gc_ref(store, value.as_ref())?;
                }
            } else {
                unsafe {
                    *global.as_u32_mut() = raw;
                }
            }
            return Ok(());
        }
    }
    write_global_snapshot(store, global, value)
}

fn ensure_global_snapshot_type(value: GlobalSnapshot, ty: WasmValType) -> Result<()> {
    let matches = match (value, ty) {
        (GlobalSnapshot::I32(_), WasmValType::I32)
        | (GlobalSnapshot::I64(_), WasmValType::I64)
        | (GlobalSnapshot::F32(_), WasmValType::F32)
        | (GlobalSnapshot::F64(_), WasmValType::F64)
        | (GlobalSnapshot::V128(_), WasmValType::V128) => true,
        (GlobalSnapshot::FuncRef(_), WasmValType::Ref(ref_ty)) => {
            matches!(ref_ty.heap_type.top(), WasmHeapTopType::Func)
        }
        (GlobalSnapshot::GcRef(_), WasmValType::Ref(ref_ty)) => matches!(
            ref_ty.heap_type.top(),
            WasmHeapTopType::Any | WasmHeapTopType::Extern | WasmHeapTopType::Exn
        ),
        (GlobalSnapshot::ExternalizedRef { .. }, WasmValType::Ref(ref_ty)) => {
            ref_ty.is_transactional_ref() && ref_ty.heap_type.top() == WasmHeapTopType::Extern
        }
        _ => false,
    };
    ensure!(
        matches,
        "transactional global value tag does not match global type"
    );
    Ok(())
}

fn global_snapshot_bytes(value: GlobalSnapshot) -> Vec<u8> {
    match value {
        GlobalSnapshot::I32(value) => value.to_ne_bytes().to_vec(),
        GlobalSnapshot::I64(value) => value.to_ne_bytes().to_vec(),
        GlobalSnapshot::F32(value) => value.to_ne_bytes().to_vec(),
        GlobalSnapshot::F64(value) => value.to_ne_bytes().to_vec(),
        GlobalSnapshot::V128(value) => value.to_vec(),
        GlobalSnapshot::GcRef(value) => value.to_ne_bytes().to_vec(),
        GlobalSnapshot::ExternalizedRef { wrapper, .. } => wrapper.to_ne_bytes().to_vec(),
        GlobalSnapshot::FuncRef(value) => value.to_ne_bytes().to_vec(),
    }
}

fn transaction_tmemory_load(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    offset: u64,
    len: u32,
) -> Result<*mut u8> {
    let result = transaction_tmemory_load_impl(store, instance, memory, addr, offset, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_load_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    offset: u64,
    len: u32,
) -> Result<*mut u8> {
    let effective = checked_tmemory_effective_address(addr, offset)?;
    let len = usize::try_from(len).context("tmemory access length overflow")?;
    flush_pending_tmemory_store(store, instance)?;
    let (memory_index, snapshot) =
        collect_defined_tmemory_snapshot(store, instance, memory, effective, len)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;

    let state = store.store_opaque_mut().transaction_state_mut();
    let bytes = state.read_tmemory_owned_from_snapshot(
        owner_instance_key,
        memory_index.as_u32(),
        effective,
        len,
        &snapshot,
    )?;
    Ok(state.set_scratch(bytes))
}

fn transaction_tmemory_store(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    offset: u64,
    len: u32,
) -> Result<*mut u8> {
    let result = transaction_tmemory_store_impl(store, instance, memory, addr, offset, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_store_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    offset: u64,
    len: u32,
) -> Result<*mut u8> {
    let effective = checked_tmemory_effective_address(addr, offset)?;
    let len = usize::try_from(len).context("tmemory access length overflow")?;
    flush_pending_tmemory_store(store, instance)?;
    let (memory_index, snapshot) =
        collect_defined_tmemory_snapshot(store, instance, memory, effective, len)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;

    let state = store.store_opaque_mut().transaction_state_mut();
    let bytes = state.read_tmemory_owned_from_snapshot(
        owner_instance_key,
        memory_index.as_u32(),
        effective,
        len,
        &snapshot,
    )?;
    state.set_tmemory_store_scratch(
        instance,
        owner_instance_key,
        memory_index.as_u32(),
        effective,
        bytes,
    )
}

fn transaction_tmemory_size(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
) -> Result<*mut u8> {
    let result = transaction_tmemory_size_impl(store, instance, memory);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_size_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;
    let pages = visible_tmemory_pages(store, instance, owner_instance_key, memory_index)?;
    Ok(usize::try_from(pages).context("transactional memory size overflow")? as *mut u8)
}

fn transaction_tmemory_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    delta: u64,
) -> Result<Option<AllocationSize>> {
    let result = transaction_tmemory_grow_impl(store, instance, memory, delta);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_grow_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    delta: u64,
) -> Result<Option<AllocationSize>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;
    let previous_pages = visible_tmemory_pages(store, instance, owner_instance_key, memory_index)?;
    let Some(new_pages) = previous_pages.checked_add(delta) else {
        return Ok(None);
    };
    let can_grow = {
        let instance_ref = store.instance_mut(instance);
        let instance_ref = instance_ref.as_ref();
        let tmemory = instance_ref
            .get_tmemory(memory_index)
            .context("transactional memory operation targeted non-transactional memory")?;
        tmemory.can_grow_to_pages(new_pages)
    };
    if !can_grow {
        return Ok(None);
    }
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_memory_size_owned(owner_instance_key, memory_index.as_u32(), new_pages)?;
    Ok(Some(AllocationSize(
        usize::try_from(previous_pages).context("tmemory previous size overflow")?,
    )))
}

fn transaction_tmemory_fill(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    dst: u64,
    val: u32,
    len: u64,
) -> Result<()> {
    let result = transaction_tmemory_fill_impl(store, instance, memory, dst, val, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_fill_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    dst: u64,
    val: u32,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let len = usize::try_from(len).context("tmemory fill length overflow")?;
    let (memory_index, snapshot) =
        collect_defined_tmemory_snapshot(store, instance, memory, dst, len)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;
    let bytes = alloc::vec![val as u8; len];
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            owner_instance_key,
            memory_index.as_u32(),
            dst,
            &bytes,
            &snapshot,
        )
}

fn transaction_tmemory_copy(
    store: &mut dyn VMStore,
    dst_instance: InstanceId,
    dst_memory: u32,
    src_vmctx: *mut u8,
    src_memory: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_tmemory_copy_impl(
        store,
        dst_instance,
        dst_memory,
        src_vmctx,
        src_memory,
        dst,
        src,
        len,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_copy_impl(
    store: &mut dyn VMStore,
    dst_instance: InstanceId,
    dst_memory: u32,
    src_vmctx: *mut u8,
    src_memory: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, dst_instance)?;
    ensure_active_transaction(store, dst_instance)?;
    let src_vmctx = NonNull::new(src_vmctx.cast::<vm::VMContext>())
        .context("tmemory.copy source VMContext is null")?;
    // SAFETY: compiled Wasm passes the live VMContext selected by the source
    // transactional-memory operand.
    let src_instance = unsafe { vm::Instance::vmctx_instance_id(src_vmctx) };
    let len = usize::try_from(len).context("tmemory copy length overflow")?;
    let (src_memory_index, src_snapshot) =
        collect_defined_tmemory_snapshot(store, src_instance, src_memory, src, len)?;
    let src_owner_instance_key =
        tmemory_transaction_owner_key(store, src_instance, src_memory_index)?;
    let bytes = store
        .store_opaque_mut()
        .transaction_state_mut()
        .read_tmemory_owned_from_snapshot(
            src_owner_instance_key,
            src_memory_index.as_u32(),
            src,
            len,
            &src_snapshot,
        )?;
    let (dst_memory_index, dst_snapshot) =
        collect_defined_tmemory_snapshot(store, dst_instance, dst_memory, dst, len)?;
    let dst_owner_instance_key =
        tmemory_transaction_owner_key(store, dst_instance, dst_memory_index)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            dst_owner_instance_key,
            dst_memory_index.as_u32(),
            dst,
            &bytes,
            &dst_snapshot,
        )
}

fn transaction_tmemory_init(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    dst: u64,
    src: u64,
    len: u64,
    data: *mut u8,
    data_len: u64,
) -> Result<()> {
    let result =
        transaction_tmemory_init_impl(store, instance, memory, dst, src, len, data, data_len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_init_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    dst: u64,
    src: u64,
    len: u64,
    data: *mut u8,
    data_len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let len = usize::try_from(len).context("tmemory init length overflow")?;
    let data_len = usize::try_from(data_len).context("tmemory init data length overflow")?;
    let src = usize::try_from(src).context("tmemory init source offset overflow")?;
    let src_end = src
        .checked_add(len)
        .context("tmemory init source range overflow")?;
    ensure!(
        src <= data_len && src_end <= data_len,
        "out of bounds tmemory access: data source range {src}..{src_end} exceeds segment length {data_len}"
    );
    let (memory_index, snapshot) =
        collect_defined_tmemory_snapshot(store, instance, memory, dst, len)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;
    let data = unsafe { data.add(src) };
    let bytes = unsafe { core::slice::from_raw_parts(data.cast_const(), len) };
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            owner_instance_key,
            memory_index.as_u32(),
            dst,
            bytes,
            &snapshot,
        )
}

fn transaction_tmemory_static_init(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    dst: u64,
    len: u64,
    data: *mut u8,
    data_len: u64,
) -> Result<()> {
    let len = usize::try_from(len).context("tmemory static init length overflow")?;
    let data_len = usize::try_from(data_len).context("tmemory static init data length overflow")?;
    ensure!(
        len <= data_len,
        "out of bounds tmemory access: data range 0..{len} exceeds segment length {data_len}"
    );
    let dst = usize::try_from(dst).context("tmemory static init destination offset overflow")?;
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(data.cast_const(), len) }
    };
    let mut instance_ref = store.instance_mut(instance);
    let Some(tmemory) = instance_ref.as_mut().get_tmemory_mut(memory_index) else {
        bail!("transactional memory operation targeted non-transactional memory");
    };
    tmemory.commit_range(dst, bytes)
}

/// Copies committed transactional-memory bytes for host APIs and diagnostics.
pub(crate) fn transactional_memory_host_read(
    store: &StoreOpaque,
    instance: InstanceId,
    memory: DefinedMemoryIndex,
    range: core::ops::Range<usize>,
) -> Result<Vec<u8>> {
    let instance_ref = store.instance(instance);
    let module = instance_ref.env_module();
    let defined = module
        .defined_tmemory_index_from_runtime(memory)
        .context("transactional memory handle refers to an ordinary memory slot")?;
    let memory = module.tmemory_index(defined);
    let tmemory = instance_ref
        .get_tmemory(memory)
        .context("transactional memory handle has no sidecar storage")?;
    tmemory.read_committed(range)
}

/// Applies a host write through the same staging and commit path as `tstore`.
pub(crate) fn transactional_memory_host_write(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: DefinedMemoryIndex,
    dst: u64,
    bytes: &[u8],
) -> Result<()> {
    let began = begin_transaction_constructor_boundary(store)?;
    let operation = (|| {
        let (memory_index, snapshot) =
            collect_defined_tmemory_snapshot(store, instance, memory.as_u32(), dst, bytes.len())?;
        let owner = tmemory_transaction_owner_key(store, instance, memory_index)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .stage_tmemory_write_owned_from_snapshot(
                owner,
                memory_index.as_u32(),
                dst,
                bytes,
                &snapshot,
            )
    })();
    if operation.is_err() {
        let _ = abort_active_transaction_on_error(store, &operation);
    }
    operation?;
    if began {
        let result = transaction_commit_impl(store, instance);
        let _ = abort_active_transaction_on_error(store, &result);
        result?;
    }
    Ok(())
}

/// Applies a host grow through the same staged size and commit path as
/// `tmemory.grow`.
pub(crate) fn transactional_memory_host_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: DefinedMemoryIndex,
    delta: u64,
) -> Result<Option<u64>> {
    let began = begin_transaction_constructor_boundary(store)?;
    let operation = transaction_tmemory_grow_impl(store, instance, memory.as_u32(), delta)
        .and_then(|size| {
            size.map(|size| {
                u64::try_from(size.0).context("transactional memory page count overflow")
            })
            .transpose()
        });
    if operation.is_err() {
        let _ = abort_active_transaction_on_error(store, &operation);
    }
    let previous = operation?;
    if began {
        let result = transaction_commit_impl(store, instance);
        let _ = abort_active_transaction_on_error(store, &result);
        result?;
    }
    Ok(previous)
}

fn transaction_tdata_drop(store: &mut dyn VMStore, instance: InstanceId, _data: u32) -> Result<()> {
    let result = transaction_tdata_drop_impl(store, instance);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tdata_drop_impl(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)
}

fn transaction_ttable_get(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
) -> Result<*mut u8> {
    let result = transaction_ttable_get_impl(store, instance, table, index);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn table_element_snapshot_from_raw(
    element_type: TableElementType,
    value: *mut u8,
) -> Result<TableElementSnapshot> {
    Ok(match element_type {
        TableElementType::Func => TableElementSnapshot::FuncRef(value.addr()),
        TableElementType::GcRef => TableElementSnapshot::GcRef(
            u32::try_from(value.addr())
                .context("transactional table GC reference does not fit u32")?,
        ),
        TableElementType::Cont => bail!("transactional contref table is not implemented yet"),
    })
}

fn transaction_table_snapshot_from_raw(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    value: *mut u8,
) -> Result<TableElementSnapshot> {
    let table_index = DefinedTableIndex::from_u32(table);
    let mut store_no_gc = AutoAssertNoGc::new(store.store_opaque_mut());
    let (_gc_store, _registry, instance_ref) =
        store_no_gc.optional_gc_store_and_registry_and_instance_mut(instance);
    let table_ref = instance_ref.get_defined_table(table_index);
    table_element_snapshot_from_raw(table_ref.element_type(), value)
}

fn normalize_persistent_table_snapshot(
    store: &mut StoreOpaque,
    snapshot: TableElementSnapshot,
) -> Result<TableElementSnapshot> {
    let TableElementSnapshot::GcRef(gc_ref) = snapshot else {
        return Ok(snapshot);
    };
    let abi = ObjectValueAbi::from_live_parts(
        OBJECT_VALUE_ABI_TAG_REF,
        u64::from(gc_ref),
        OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    )?;
    let value = object_value_from_persistent_slot_abi(store, abi)?;
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    Ok(TableElementSnapshot::GcRef(
        gc_snapshot_raw_from_object_value(
            &*durable_refs,
            state,
            object_table,
            value,
            "transactional table GC reference",
        )?,
    ))
}

fn gc_snapshot_raw_from_object_value(
    durable_refs: &DurableReferenceRegistry,
    state: &mut TransactionState,
    object_table: &mut ObjectTable,
    value: ObjectValue,
    context: &str,
) -> Result<u32> {
    match value {
        ObjectValue::Ref(Some(object_id)) => state
            .transaction_ref_handle_for_object_id_avoiding(object_table, object_id, |raw| {
                live_ref_raw_is_registered(durable_refs, raw)
            }),
        ObjectValue::Ref(None) => Ok(0),
        ObjectValue::I31(value) => u32::try_from(ObjectTable::encode_raw_i31_ref(value))
            .with_context(|| format!("{context} i31 reference does not fit u32")),
        ObjectValue::ExternRef(identity) => durable_refs
            .resolve_extern_identity(identity)
            .with_context(|| {
                format!(
                    "{context} durable external identity is not registered in this store: namespace={} handle={:#x} layout={}",
                    identity.namespace,
                    identity.handle,
                    identity.type_layout_id.get()
                )
            }),
        _ => bail!("{context} did not normalize to a GC reference"),
    }
}

fn table_element_snapshot_to_raw(value: TableElementSnapshot) -> *mut u8 {
    match value {
        TableElementSnapshot::FuncRef(value) => core::ptr::with_exposed_provenance_mut(value),
        TableElementSnapshot::GcRef(value) => {
            core::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap())
        }
    }
}

fn live_transaction_table_snapshot_for_read(
    store: &mut dyn VMStore,
    snapshot: TableElementSnapshot,
) -> Result<TableElementSnapshot> {
    let TableElementSnapshot::GcRef(gc_ref) = snapshot else {
        return Ok(snapshot);
    };
    Ok(TableElementSnapshot::GcRef(
        live_transaction_gc_ref_for_read(store, gc_ref)?,
    ))
}

fn stage_transaction_table_element_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    snapshot: TableElementSnapshot,
) -> Result<()> {
    ensure_transaction_table_index_in_bounds(store, instance, table, index)?;
    let snapshot = normalize_persistent_table_snapshot(store.store_opaque_mut(), snapshot)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_table_element_owned(Some(instance), table, index, snapshot)?;
    Ok(())
}

fn write_or_stage_transaction_table_element_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    snapshot: TableElementSnapshot,
) -> Result<()> {
    if store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some()
    {
        return stage_transaction_table_element_snapshot(store, instance, table, index, snapshot);
    }
    write_table_element_snapshot(store, instance, table, index, snapshot)
}

fn write_table_element_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: TableElementSnapshot,
) -> Result<()> {
    ensure_defined_table_index_in_bounds(store, instance, table, index)?;
    let table_index = DefinedTableIndex::from_u32(table);
    let mut store = AutoAssertNoGc::new(store.store_opaque_mut());
    let (gc_store, _registry, instance_ref) =
        store.optional_gc_store_and_registry_and_instance_mut(instance);
    let table_ref = instance_ref.get_defined_table(table_index);
    match (table_ref.element_type(), value) {
        (TableElementType::Func, TableElementSnapshot::FuncRef(value)) => {
            let ptr = core::ptr::with_exposed_provenance_mut::<vm::VMFuncRef>(value);
            table_ref.set_func(index, NonNull::new(ptr))?;
        }
        (TableElementType::GcRef, TableElementSnapshot::GcRef(value)) => {
            let elem = VMGcRef::from_raw_u32(value);
            table_ref.set_gc_ref(gc_store, index, elem.as_ref())?;
        }
        (TableElementType::Cont, _) => bail!("transactional contref table is not implemented yet"),
        _ => bail!("transactional table element kind does not match table element type"),
    }
    Ok(())
}

fn transaction_ttable_get_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let visible_size = transaction_table_size_for_bounds(store, instance, table)?;
    if index >= visible_size {
        bail!(Trap::TableOutOfBounds);
    }
    let (staged, visibility) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_table_granule_read_owned(Some(instance), table, index, 0)?;
        (
            state.staged_table_element_owned(Some(instance), table, index),
            state.active_visibility_read_context()?,
        )
    };
    if let Some(value) = staged {
        let value = live_transaction_table_snapshot_for_read(store, value)?;
        return Ok(table_element_snapshot_to_raw(value));
    }
    let snapshot_visible_size = snapshot_visible_ttable_size(store, instance, table)?;
    ensure!(
        index < snapshot_visible_size,
        "transactional table overlay is missing staged element {index} in grown table"
    );
    let granule_index = index / TableGranuleSnapshot::ELEMENT_CAPACITY;
    let granule = GranuleId::TTable {
        instance: Some(instance.as_u32()),
        table_index: table,
        granule_index,
    };
    let visible = visibility.read_table(granule, || {
        collect_current_table_granule(store, instance, table, granule_index, snapshot_visible_size)
    })?;
    let mut elements = visible.into_elements();
    let granule_start = granule_index
        .checked_mul(TableGranuleSnapshot::ELEMENT_CAPACITY)
        .context("ttable granule start overflow")?;
    let remaining = snapshot_visible_size
        .checked_sub(granule_start)
        .context("snapshot-visible table granule starts beyond table size")?;
    let expected_len = usize::try_from(remaining.min(TableGranuleSnapshot::ELEMENT_CAPACITY))
        .context("snapshot-visible table granule length does not fit host usize")?;
    ensure!(
        elements.len() == expected_len,
        "snapshot-visible table granule length mismatch: expected {expected_len} elements, got {}",
        elements.len()
    );
    {
        let state = store.store_opaque_mut().transaction_state_mut();
        for (offset, element) in elements.iter_mut().enumerate() {
            let element_index = granule_start
                .checked_add(u64::try_from(offset).unwrap())
                .context("ttable element index overflow")?;
            if let Some(staged) =
                state.staged_table_element_owned(Some(instance), table, element_index)
            {
                *element = staged;
            }
        }
    }
    let offset = usize::try_from(index - granule_start).unwrap();
    let value = elements
        .get(offset)
        .copied()
        .context("snapshot-visible table granule is missing an in-bounds element")?;
    let value = live_transaction_table_snapshot_for_read(store, value)?;
    Ok(table_element_snapshot_to_raw(value))
}

fn transaction_ttable_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_ttable_set_impl(store, instance, table, index, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let snapshot = transaction_table_snapshot_from_raw(store, instance, table, value)?;
    stage_transaction_table_element_snapshot(store, instance, table, index, snapshot)
}

fn transaction_ttable_startup_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_ttable_startup_set_impl(store, instance, table, index, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_startup_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    let snapshot = transaction_table_snapshot_from_raw(store, instance, table, value)?;
    write_or_stage_transaction_table_element_snapshot(store, instance, table, index, snapshot)
}

fn transaction_ttable_startup_fill(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_ttable_startup_fill_impl(store, instance, table, start, len, value);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_startup_fill_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, start, len)?;
    let snapshot = transaction_table_snapshot_from_raw(store, instance, table, value)?;
    if store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some()
    {
        let snapshot = normalize_persistent_table_snapshot(store.store_opaque_mut(), snapshot)?;
        for element_index in start..start + len {
            store
                .store_opaque_mut()
                .transaction_state_mut()
                .stage_table_element_owned(Some(instance), table, element_index, snapshot)?;
        }
        return Ok(());
    }
    for element_index in start..start + len {
        write_table_element_snapshot(store, instance, table, element_index, snapshot)?;
    }
    Ok(())
}

fn transaction_ttable_startup_check_range(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_startup_check_range_impl(store, instance, table, start, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_startup_check_range_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, start, len)
}

fn transaction_ttable_read_range(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_read_range_impl(store, instance, table, start, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_read_range_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, start, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .acquire_table_granule_read_range_owned(Some(instance), table, start, len, 0)?;
    Ok(())
}

fn transaction_ttable_write_range(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_write_range_impl(store, instance, table, start, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_write_range_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, start, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .acquire_table_granule_write_range_owned(Some(instance), table, start, len, 0)?;
    Ok(())
}

fn transaction_ttable_copy(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_table: u32,
    src_table: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_copy_impl(store, instance, dst_table, src_table, dst, src, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_copy_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_table: u32,
    src_table: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, src_table, src, len)?;
    ensure_transaction_table_range_in_bounds(store, instance, dst_table, dst, len)?;
    let mut values = Vec::with_capacity(usize::try_from(len)?);
    for offset in 0..len {
        values.push(transaction_ttable_get_impl(
            store,
            instance,
            src_table,
            src.checked_add(offset)
                .context("ttable.copy source overflow")?,
        )?);
    }
    for (offset, value) in values.into_iter().enumerate() {
        transaction_ttable_set_impl(
            store,
            instance,
            dst_table,
            dst.checked_add(u64::try_from(offset)?)
                .context("ttable.copy destination overflow")?,
            value,
        )?;
    }
    Ok(())
}

fn transaction_ttable_init(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    elem: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_init_impl(store, instance, table, elem, dst, src, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_init_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    elem: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, dst, len)?;
    let src = usize::try_from(src).context("ttable.init source does not fit host usize")?;
    let len = usize::try_from(len).context("ttable.init length does not fit host usize")?;
    if elem == u32::MAX {
        ensure!(
            src == 0 && len == 0,
            "out of bounds transactional element segment access"
        );
        return Ok(());
    }
    let values = {
        let mut instance_ref = store.instance_mut(instance);
        let segment = instance_ref
            .as_mut()
            .passive_telement_segment(PassiveTElemIndex::from_u32(elem));
        let end = src
            .checked_add(len)
            .context("ttable.init element range overflow")?;
        ensure!(
            end <= segment.len(),
            "out of bounds transactional element segment access"
        );
        segment[src..end]
            .iter()
            .map(|value| value.get_funcref().cast::<u8>())
            .collect::<Vec<_>>()
    };
    for (offset, value) in values.into_iter().enumerate() {
        transaction_ttable_set_impl(
            store,
            instance,
            table,
            dst.checked_add(u64::try_from(offset)?)
                .context("ttable.init destination overflow")?,
            value,
        )?;
    }
    Ok(())
}

fn transaction_ttable_size(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<*mut u8> {
    let result = transaction_ttable_size_impl(store, instance, table);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_size_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let size = visible_ttable_size(store, instance, table)?;
    Ok(usize::try_from(size).context("transactional table size overflow")? as *mut u8)
}

fn transaction_ttable_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    delta: u64,
    init: *mut u8,
) -> Result<Option<AllocationSize>> {
    let result = transaction_ttable_grow_impl(store, instance, table, delta, init);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_grow_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    delta: u64,
    init: *mut u8,
) -> Result<Option<AllocationSize>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;

    let table_index = DefinedTableIndex::from_u32(table);
    let (maximum, element_type) = {
        let mut store = AutoAssertNoGc::new(store.store_opaque_mut());
        let (_gc_store, _registry, instance_ref) =
            store.optional_gc_store_and_registry_and_instance_mut(instance);
        let table_ref = instance_ref.get_defined_table(table_index);
        (
            table_ref.maximum().map(u64::try_from).transpose()?,
            table_ref.element_type(),
        )
    };
    let init = table_element_snapshot_from_raw(element_type, init)?;
    let init = normalize_persistent_table_snapshot(store.store_opaque_mut(), init)?;
    let current_size = visible_ttable_size(store, instance, table)?;
    let new_size = current_size
        .checked_add(delta)
        .context("transactional table grow size overflow")?;
    if new_size > u64::from(u32::MAX) || maximum.is_some_and(|maximum| new_size > maximum) {
        return Ok(None);
    }

    let state = store.store_opaque_mut().transaction_state_mut();
    state.stage_table_size_owned(Some(instance), table, new_size)?;
    for element_index in current_size..new_size {
        state.stage_table_element_owned(Some(instance), table, element_index, init)?;
    }

    Ok(Some(AllocationSize(
        usize::try_from(current_size).context("transactional table size overflow")?,
    )))
}

fn transaction_tstruct_new(
    store: &mut dyn VMStore,
    instance: InstanceId,
    struct_type: u32,
    field_count: u32,
    fields: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tstruct_new_impl(store, instance, struct_type, field_count, fields);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )
}

fn transaction_tstruct_new_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    struct_type: u32,
    field_count: u32,
    fields: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, struct_type)?;
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let field_count =
        usize::try_from(field_count).context("transactional struct field count overflow")?;
    ensure!(
        field_count == 0 || !fields.is_null(),
        "transactional struct fields pointer is null"
    );
    let fields = if field_count == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(fields.cast::<ObjectValueAbi>(), field_count) }
    };

    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let mut values = Vec::with_capacity(field_count);
    for abi in fields {
        values.push(object_value_from_transaction_abi(
            durable_refs,
            Some(&*state),
            object_table,
            *abi,
        )?);
    }
    let object_id = state.allocate_transaction_local_struct(values, Some(runtime_type_index))?;
    transaction_object_ref_handle_for_object_id(durable_refs, state, object_table, object_id)
}

fn transaction_tstruct_static_new(
    store: &mut dyn VMStore,
    instance: InstanceId,
    struct_type: u32,
    field_count: u32,
    fields: *mut u8,
    layout_fields: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let result = transaction_tstruct_static_new_impl(
        store,
        instance,
        struct_type,
        field_count,
        fields,
        layout_fields,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tstruct_static_new_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    struct_type: u32,
    field_count: u32,
    fields: *mut u8,
    layout_fields: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, struct_type)?;
    let field_count =
        usize::try_from(field_count).context("transactional struct field count overflow")?;
    ensure!(
        field_count == 0 || !fields.is_null(),
        "transactional struct fields pointer is null"
    );
    ensure!(
        field_count == 0 || !layout_fields.is_null(),
        "transactional struct layout fields pointer is null"
    );
    let fields = if field_count == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(fields.cast::<ObjectValueAbi>(), field_count) }
    };
    let layout_fields = if field_count == 0 {
        &[][..]
    } else {
        unsafe {
            core::slice::from_raw_parts(
                layout_fields.cast::<WasmtimePersistentFieldLayoutAbi>(),
                field_count,
            )
        }
    };

    let active_transaction = store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some();
    if active_transaction {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        let mut values = Vec::with_capacity(field_count);
        for abi in fields {
            values.push(object_value_from_transaction_abi(
                durable_refs,
                Some(&*state),
                object_table,
                *abi,
            )?);
        }
        for abi in layout_fields {
            abi.to_field_layout()?;
        }
        let object_id =
            state.allocate_transaction_local_struct(values, Some(runtime_type_index))?;
        return transaction_object_ref_handle_for_object_id(
            durable_refs,
            state,
            object_table,
            object_id,
        );
    }

    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let mut values = Vec::with_capacity(field_count);
    for abi in fields {
        values.push(object_value_from_transaction_abi(
            durable_refs,
            None,
            object_table,
            *abi,
        )?);
    }
    let mut field_layouts = Vec::with_capacity(field_count);
    for abi in layout_fields {
        field_layouts.push(abi.to_field_layout()?);
    }
    let object_id = object_table.allocate_persistent_struct_with_wasmtime_type_layout(
        Some(instance),
        struct_type,
        field_layouts,
        values,
    )?;
    object_table.set_runtime_type_index(object_id, runtime_type_index)?;
    transaction_object_ref_handle_for_shared_object_id(durable_refs, object_table, object_id)
}

fn transaction_tstruct_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    field: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<()> {
    let result = transaction_tstruct_set_impl(store, instance, gc_ref, field, tag, low, high);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tstruct_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    field: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let field = usize::try_from(field).context("transactional struct field index overflow")?;
    let value = object_value_from_persistent_slot_abi(store.store_opaque_mut(), abi)?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
    state.stage_struct_field(object_table, object_id, field, value)
}

fn transaction_tstruct_get(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    field: u32,
) -> Result<*mut u8> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tstruct_get_bytes_impl(store, instance, gc_ref, field);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    let bytes = combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )?;
    Ok(store
        .store_opaque_mut()
        .transaction_state_mut()
        .set_scratch(bytes))
}

fn transaction_tstruct_get_bytes_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    field: u32,
) -> Result<Vec<u8>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let field = usize::try_from(field).context("transactional struct field index overflow")?;
    let value = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
        state.read_struct_field(object_table, object_id, field)?
    };
    let abi = {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        live_transaction_abi_from_object_value(durable_refs, state, object_table, &value)?
    };
    Ok(object_value_abi_bytes(abi))
}

fn transaction_tarray_new(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    len: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<core::num::NonZeroU32> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tarray_new_impl(store, instance, array_type, len, tag, low, high);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )
}

fn transaction_tarray_new_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    len: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let value = object_value_from_transaction_abi(durable_refs, Some(&*state), object_table, abi)?;
    allocate_transaction_array_record(
        durable_refs,
        state,
        object_table,
        vec![value; len],
        Some(runtime_type_index),
    )
}

fn transaction_tarray_static_new(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_size: u32,
    element_is_object_ref: u32,
    len: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<core::num::NonZeroU32> {
    let result = transaction_tarray_static_new_impl(
        store,
        instance,
        array_type,
        element_size,
        element_is_object_ref,
        len,
        tag,
        low,
        high,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_static_new_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_size: u32,
    element_is_object_ref: u32,
    len: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    ensure!(
        element_is_object_ref <= 1,
        "transactional array element kind flag must be 0 or 1"
    );
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let active_transaction = store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some();
    if active_transaction {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        let value =
            object_value_from_transaction_abi(durable_refs, Some(&*state), object_table, abi)?;
        return allocate_transaction_array_record(
            durable_refs,
            state,
            object_table,
            vec![value; len],
            Some(runtime_type_index),
        );
    }

    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let value = object_value_from_transaction_abi(durable_refs, None, object_table, abi)?;
    let object_id = object_table
        .allocate_persistent_array_with_wasmtime_element_layout_and_initializer(
            Some(instance),
            array_type,
            element_size,
            element_is_object_ref != 0,
            value,
            len,
        )?;
    object_table.set_runtime_type_index(object_id, runtime_type_index)?;
    transaction_object_ref_handle_for_shared_object_id(durable_refs, object_table, object_id)
}

fn transaction_tarray_new_fixed(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_count: u32,
    elements: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result =
        transaction_tarray_new_fixed_impl(store, instance, array_type, element_count, elements);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )
}

fn transaction_tarray_new_fixed_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_count: u32,
    elements: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let element_count =
        usize::try_from(element_count).context("transactional array element count overflow")?;
    ensure!(
        element_count == 0 || !elements.is_null(),
        "transactional array elements pointer is null"
    );
    let elements = if element_count == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(elements.cast::<ObjectValueAbi>(), element_count) }
    };

    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let mut values = Vec::with_capacity(element_count);
    for abi in elements {
        values.push(object_value_from_transaction_abi(
            durable_refs,
            Some(&*state),
            object_table,
            *abi,
        )?);
    }
    allocate_transaction_array_record(
        durable_refs,
        state,
        object_table,
        values,
        Some(runtime_type_index),
    )
}

fn transaction_tarray_static_new_fixed(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_size: u32,
    element_is_object_ref: u32,
    element_count: u32,
    elements: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let result = transaction_tarray_static_new_fixed_impl(
        store,
        instance,
        array_type,
        element_size,
        element_is_object_ref,
        element_count,
        elements,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_static_new_fixed_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    element_size: u32,
    element_is_object_ref: u32,
    element_count: u32,
    elements: *mut u8,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    ensure!(
        element_is_object_ref <= 1,
        "transactional array fixed element kind flag must be 0 or 1"
    );
    let element_count =
        usize::try_from(element_count).context("transactional array element count overflow")?;
    ensure!(
        element_count == 0 || !elements.is_null(),
        "transactional array elements pointer is null"
    );
    let elements = if element_count == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(elements.cast::<ObjectValueAbi>(), element_count) }
    };

    let active_transaction = store
        .store_opaque()
        .transaction_state()
        .active_transaction()
        .is_some();
    if active_transaction {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        let mut values = Vec::with_capacity(element_count);
        for abi in elements {
            values.push(object_value_from_transaction_abi(
                durable_refs,
                Some(&*state),
                object_table,
                *abi,
            )?);
        }
        return allocate_transaction_array_record(
            durable_refs,
            state,
            object_table,
            values,
            Some(runtime_type_index),
        );
    }

    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let mut values = Vec::with_capacity(element_count);
    for abi in elements {
        values.push(object_value_from_transaction_abi(
            durable_refs,
            None,
            object_table,
            *abi,
        )?);
    }
    let namespace = instance
        .as_u32()
        .checked_add(1)
        .context("transactional instance namespace overflow")?;
    let object_id = object_table.allocate_persistent_array_with_wasmtime_fixed_type_namespace(
        namespace,
        array_type,
        element_size,
        element_is_object_ref != 0,
        values,
    )?;
    object_table.set_runtime_type_index(object_id, runtime_type_index)?;
    transaction_object_ref_handle_for_shared_object_id(durable_refs, object_table, object_id)
}

fn transaction_tarray_new_data(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    src: u32,
    len: u32,
    data: *mut u8,
    data_len: u64,
    tag: u32,
    element_size: u32,
) -> Result<core::num::NonZeroU32> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tarray_new_data_impl(
        store,
        instance,
        array_type,
        src,
        len,
        data,
        data_len,
        tag,
        element_size,
    );
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )
}

fn transaction_tarray_new_data_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    src: u32,
    len: u32,
    data: *mut u8,
    data_len: u64,
    tag: u32,
    element_size: u32,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let src = usize::try_from(src).context("transactional array data source offset overflow")?;
    let len = usize::try_from(len).context("transactional array data length overflow")?;
    let data_len =
        usize::try_from(data_len).context("transactional array data segment length overflow")?;
    let element_size =
        usize::try_from(element_size).context("transactional array element size overflow")?;
    ensure!(element_size > 0, "transactional array element size is zero");
    let byte_start = src
        .checked_mul(element_size)
        .context("transactional array data source byte offset overflow")?;
    let byte_len = len
        .checked_mul(element_size)
        .context("transactional array data byte length overflow")?;
    let byte_end = byte_start
        .checked_add(byte_len)
        .context("transactional array data source range overflow")?;
    ensure!(
        byte_start <= data_len && byte_end <= data_len,
        "out of bounds tarray access: data source range {byte_start}..{byte_end} exceeds segment length {data_len}"
    );
    let bytes = unsafe { core::slice::from_raw_parts(data.add(byte_start).cast_const(), byte_len) };
    let values = decode_transaction_array_data_values(bytes, tag, element_size)?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    allocate_transaction_array_record(
        durable_refs,
        state,
        object_table,
        values,
        Some(runtime_type_index),
    )
}

fn transaction_tarray_new_elem(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    src: u32,
    len: u32,
    elem: *mut u8,
    elem_len: u64,
) -> Result<core::num::NonZeroU32> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result =
        transaction_tarray_new_elem_impl(store, instance, array_type, src, len, elem, elem_len);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )
}

fn transaction_tarray_new_elem_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    array_type: u32,
    src: u32,
    len: u32,
    elem: *mut u8,
    elem_len: u64,
) -> Result<core::num::NonZeroU32> {
    let runtime_type_index = transaction_module_type_index_to_shared(store, instance, array_type)?;
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let src = usize::try_from(src).context("transactional array elem source offset overflow")?;
    let len = usize::try_from(len).context("transactional array elem length overflow")?;
    let elem_len =
        usize::try_from(elem_len).context("transactional elem segment length overflow")?;
    ensure!(
        len == 0 || !elem.is_null(),
        "transactional elem segment pointer is null"
    );
    let elem_end = src
        .checked_add(len)
        .context("transactional array elem source range overflow")?;
    ensure!(
        src <= elem_len && elem_end <= elem_len,
        "out of bounds tarray access: elem source range {src}..{elem_end} exceeds segment length {elem_len}"
    );

    let byte_start = src
        .checked_mul(16)
        .context("transactional array elem byte offset overflow")?;
    let byte_len = len
        .checked_mul(16)
        .context("transactional array elem byte length overflow")?;
    let bytes = if byte_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(elem.add(byte_start).cast_const(), byte_len) }
    };
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let values = decode_transaction_array_elem_values(durable_refs, &*state, object_table, bytes)?;
    allocate_transaction_array_record(
        durable_refs,
        state,
        object_table,
        values,
        Some(runtime_type_index),
    )
}

fn decode_transaction_array_data_values(
    bytes: &[u8],
    tag: u32,
    element_size: usize,
) -> Result<Vec<ObjectValue>> {
    ensure!(
        bytes.len() % element_size == 0,
        "transactional array data bytes are not element aligned"
    );
    bytes
        .chunks_exact(element_size)
        .map(|chunk| match (tag, element_size) {
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_I32, 1) => {
                Ok(ObjectValue::I32(i32::from(chunk[0])))
            }
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_I32, 2) => Ok(ObjectValue::I32(
                i32::from(u16::from_le_bytes(chunk.try_into().unwrap())),
            )),
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_I32, 4) => Ok(ObjectValue::I32(
                i32::from_le_bytes(chunk.try_into().unwrap()),
            )),
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_I64, 8) => Ok(ObjectValue::I64(
                i64::from_le_bytes(chunk.try_into().unwrap()),
            )),
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_F32, 4) => Ok(ObjectValue::F32(
                u32::from_le_bytes(chunk.try_into().unwrap()),
            )),
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_F64, 8) => Ok(ObjectValue::F64(
                u64::from_le_bytes(chunk.try_into().unwrap()),
            )),
            (crate::runtime::transaction::OBJECT_VALUE_ABI_TAG_V128, 16) => {
                let mut value = [0; 16];
                value.copy_from_slice(chunk);
                Ok(ObjectValue::V128(value))
            }
            _ => bail!("transactional array data type is not numeric or vector"),
        })
        .collect()
}

fn decode_transaction_array_elem_values(
    durable_refs: &mut DurableReferenceRegistry,
    state: &TransactionState,
    object_table: &mut ObjectTable,
    bytes: &[u8],
) -> Result<Vec<ObjectValue>> {
    ensure!(
        bytes.len() % 16 == 0,
        "transactional elem segment bytes are not ValRaw aligned"
    );
    bytes
        .chunks_exact(16)
        .map(|chunk| {
            let raw = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
            live_ref_value_from_raw(
                durable_refs,
                Some(state),
                object_table,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED,
                raw,
            )
        })
        .collect()
}

fn allocate_transaction_array_record(
    durable_refs: &DurableReferenceRegistry,
    state: &mut TransactionState,
    object_table: &mut ObjectTable,
    values: Vec<ObjectValue>,
    runtime_type_index: Option<VMSharedTypeIndex>,
) -> Result<core::num::NonZeroU32> {
    let object_id = state.allocate_transaction_local_array(values, runtime_type_index)?;
    transaction_object_ref_handle_for_object_id(durable_refs, state, object_table, object_id)
}

fn transaction_object_ref_handle_for_object_id(
    durable_refs: &DurableReferenceRegistry,
    state: &mut TransactionState,
    object_table: &mut ObjectTable,
    object_id: crate::runtime::transaction::ObjectId,
) -> Result<core::num::NonZeroU32> {
    let handle =
        state.transaction_ref_handle_for_object_id_avoiding(object_table, object_id, |raw| {
            live_ref_raw_is_registered(durable_refs, raw)
        })?;
    core::num::NonZeroU32::new(handle).context("transaction object ref handle cannot be zero")
}

fn transaction_object_ref_handle_for_shared_object_id(
    durable_refs: &DurableReferenceRegistry,
    object_table: &mut ObjectTable,
    object_id: crate::runtime::transaction::ObjectId,
) -> Result<core::num::NonZeroU32> {
    let handle = object_table.transaction_ref_handle_for_object_id_avoiding(object_id, |raw| {
        live_ref_raw_is_registered(durable_refs, raw)
    })?;
    core::num::NonZeroU32::new(handle).context("transaction object ref handle cannot be zero")
}

fn transaction_tarray_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<()> {
    let result = transaction_tarray_set_impl(store, instance, gc_ref, index, tag, low, high);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_set_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
    tag: u32,
    low: u64,
    high: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure!(gc_ref != 0, "null tarray reference");
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let value = object_value_from_persistent_slot_abi(store.store_opaque_mut(), abi)?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
    state.stage_array_element(object_table, object_id, index, value)
}

fn transaction_tarray_fill(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
    tag: u32,
    low: u64,
    high: u64,
    len: u32,
) -> Result<()> {
    let result = transaction_tarray_fill_impl(store, instance, gc_ref, index, tag, low, high, len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_fill_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
    tag: u32,
    low: u64,
    high: u64,
    len: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let value = object_value_from_persistent_slot_abi(store.store_opaque_mut(), abi)?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    ensure!(gc_ref != 0, "null tarray reference");
    let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
    state.fill_array_range(object_table, object_id, index, len, value)
}

fn transaction_tarray_copy(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_gc_ref: u32,
    dst_index: u32,
    src_gc_ref: u32,
    src_index: u32,
    len: u32,
) -> Result<()> {
    let result = transaction_tarray_copy_impl(
        store, instance, dst_gc_ref, dst_index, src_gc_ref, src_index, len,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_copy_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_gc_ref: u32,
    dst_index: u32,
    src_gc_ref: u32,
    src_index: u32,
    len: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure!(dst_gc_ref != 0, "null tarray reference");
    ensure!(src_gc_ref != 0, "null tarray reference");
    let dst_index = usize::try_from(dst_index).context("transactional array index overflow")?;
    let src_index = usize::try_from(src_index).context("transactional array index overflow")?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    let dst_object_id = state.object_id_for_transaction_ref_handle(object_table, dst_gc_ref)?;
    let src_object_id = state.object_id_for_transaction_ref_handle(object_table, src_gc_ref)?;
    state.copy_array_range(
        object_table,
        dst_object_id,
        dst_index,
        src_object_id,
        src_index,
        len,
    )
}

fn transaction_tarray_init_data(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    dst: u32,
    src: u32,
    len: u32,
    data: *mut u8,
    data_len: u64,
    tag: u32,
    element_size: u32,
) -> Result<()> {
    let result = transaction_tarray_init_data_impl(
        store,
        instance,
        gc_ref,
        dst,
        src,
        len,
        data,
        data_len,
        tag,
        element_size,
    );
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_init_data_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    dst: u32,
    src: u32,
    len: u32,
    data: *mut u8,
    data_len: u64,
    tag: u32,
    element_size: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure!(gc_ref != 0, "null tarray reference");
    let dst = usize::try_from(dst).context("transactional array destination offset overflow")?;
    let src = usize::try_from(src).context("transactional array data source offset overflow")?;
    let len = usize::try_from(len).context("transactional array data length overflow")?;
    let data_len =
        usize::try_from(data_len).context("transactional array data segment length overflow")?;
    let element_size =
        usize::try_from(element_size).context("transactional array element size overflow")?;
    ensure!(element_size > 0, "transactional array element size is zero");
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
    let dst_end = dst
        .checked_add(len)
        .context("transactional array destination range overflow")?;
    ensure!(
        dst_end <= state.read_array_len(object_table, object_id)?,
        "out of bounds array access"
    );
    let byte_start = src;
    let byte_len = len
        .checked_mul(element_size)
        .context("transactional array data byte length overflow")?;
    let byte_end = byte_start
        .checked_add(byte_len)
        .context("transactional array data source range overflow")?;
    ensure!(
        byte_start <= data_len && byte_end <= data_len,
        "out of bounds tmemory access"
    );
    let bytes = if byte_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(data.add(byte_start).cast_const(), byte_len) }
    };
    let values = decode_transaction_array_data_values(bytes, tag, element_size)?;
    state.write_array_range(object_table, object_id, dst, values)
}

fn transaction_tarray_init_elem(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    dst: u32,
    src: u32,
    len: u32,
    elem: *mut u8,
    elem_len: u64,
) -> Result<()> {
    let result =
        transaction_tarray_init_elem_impl(store, instance, gc_ref, dst, src, len, elem, elem_len);
    let _ = abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tarray_init_elem_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    dst: u32,
    src: u32,
    len: u32,
    elem: *mut u8,
    elem_len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    ensure!(gc_ref != 0, "null tarray reference");
    let dst = usize::try_from(dst).context("transactional array destination offset overflow")?;
    let src = usize::try_from(src).context("transactional array elem source offset overflow")?;
    let len = usize::try_from(len).context("transactional array elem length overflow")?;
    let elem_len =
        usize::try_from(elem_len).context("transactional elem segment length overflow")?;
    ensure!(
        len == 0 || !elem.is_null(),
        "transactional elem segment pointer is null"
    );

    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
    let dst_end = dst
        .checked_add(len)
        .context("transactional array destination range overflow")?;
    ensure!(
        dst_end <= state.read_array_len(object_table, object_id)?,
        "out of bounds array access"
    );
    let elem_end = src
        .checked_add(len)
        .context("transactional array elem source range overflow")?;
    ensure!(
        src <= elem_len && elem_end <= elem_len,
        "out of bounds table access"
    );

    let byte_start = src
        .checked_mul(16)
        .context("transactional array elem byte offset overflow")?;
    let byte_len = len
        .checked_mul(16)
        .context("transactional array elem byte length overflow")?;
    let bytes = if byte_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(elem.add(byte_start).cast_const(), byte_len) }
    };
    let values = decode_transaction_array_elem_values(durable_refs, &*state, object_table, bytes)?;
    state.write_array_range(object_table, object_id, dst, values)
}

fn transaction_tarray_get(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
) -> Result<*mut u8> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tarray_get_bytes_impl(store, instance, gc_ref, index);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    let bytes = combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )?;
    Ok(store
        .store_opaque_mut()
        .transaction_state_mut()
        .set_scratch(bytes))
}

fn transaction_tarray_get_bytes_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
    index: u32,
) -> Result<Vec<u8>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let value = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
        state.read_array_element(object_table, object_id, index)?
    };
    let abi = {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        live_transaction_abi_from_object_value(durable_refs, state, object_table, &value)?
    };
    Ok(object_value_abi_bytes(abi))
}

fn transaction_tarray_len(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
) -> Result<*mut u8> {
    let began = begin_transaction_constructor_boundary(store)?;
    let result = transaction_tarray_len_bytes_impl(store, instance, gc_ref);
    let finish = finish_transaction_constructor_boundary(store, began, &result);
    let bytes = combine_operation_and_cleanup_results(
        result,
        finish,
        "failed to finish transaction constructor boundary",
    )?;
    Ok(store
        .store_opaque_mut()
        .transaction_state_mut()
        .set_scratch(bytes))
}

fn transaction_tarray_len_bytes_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    gc_ref: u32,
) -> Result<Vec<u8>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store, instance)?;
    let len = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let object_id = state.object_id_for_transaction_ref_handle(object_table, gc_ref)?;
        state.read_array_len(object_table, object_id)?
    };
    let len = u32::try_from(len).context("transactional array length does not fit i32")?;
    let abi = ObjectValueAbi::from_object_value(&ObjectValue::I32(i32::from_ne_bytes(
        len.to_ne_bytes(),
    )))?;
    Ok(object_value_abi_bytes(abi))
}

fn object_value_from_transaction_abi(
    durable_refs: &mut DurableReferenceRegistry,
    state: Option<&TransactionState>,
    object_table: &mut ObjectTable,
    abi: ObjectValueAbi,
) -> Result<ObjectValue> {
    let (tag, low, high) = abi.as_parts();
    if tag != OBJECT_VALUE_ABI_TAG_REF {
        return abi.to_object_value();
    }
    live_ref_value_from_raw(durable_refs, state, object_table, high, low)
}

fn object_value_from_persistent_slot_abi(
    store: &mut StoreOpaque,
    abi: ObjectValueAbi,
) -> Result<ObjectValue> {
    let (tag, low, high) = abi.as_parts();
    if tag != OBJECT_VALUE_ABI_TAG_REF {
        return abi.to_object_value();
    }

    let (engine, gc_store, durable_refs, state, object_table) =
        store.transaction_promotion_context_mut();

    if high != OBJECT_VALUE_ABI_LIVE_REF_KIND_GC {
        return live_ref_value_from_raw(durable_refs, Some(&*state), object_table, high, low);
    }

    if low == 0 {
        return Ok(ObjectValue::Ref(None));
    }
    if ObjectTable::is_raw_i31_ref(low) {
        return Ok(ObjectValue::I31(ObjectTable::decode_raw_i31_ref(low)?));
    }

    let gc_ref = u32::try_from(low).context("live GC reference does not fit u32")?;
    if let Some(value) = live_object_ref_value_from_ambiguous_raw(
        durable_refs,
        Some(&*state),
        object_table,
        gc_ref,
        false,
    )? {
        return Ok(value);
    }

    let promoted = {
        let mut adapter =
            StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state.promote_gc_ref_for_live_transaction_ref_with_adapter(
            object_table,
            gc_ref,
            &mut adapter,
        )?
    };

    if let Some(object_id) = promoted {
        return Ok(ObjectValue::Ref(Some(object_id)));
    }
    if let Some(value) = live_object_ref_value_from_ambiguous_raw(
        durable_refs,
        Some(&*state),
        object_table,
        gc_ref,
        false,
    )? {
        return Ok(value);
    }
    bail!(
        "ordinary live GC reference {gc_ref:#x} did not promote to a persistent object or durable leaf"
    )
}

fn live_transaction_abi_from_object_value(
    durable_refs: &DurableReferenceRegistry,
    state: &mut TransactionState,
    object_table: &mut ObjectTable,
    value: &ObjectValue,
) -> Result<ObjectValueAbi> {
    if let ObjectValue::I31(value) = value {
        return ObjectValueAbi::from_live_parts(
            OBJECT_VALUE_ABI_TAG_REF,
            ObjectTable::encode_raw_i31_ref(*value),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_I31,
        );
    }
    let (raw, kind) = match value {
        ObjectValue::Ref(Some(object_id)) => {
            let handle =
                state.transaction_ref_handle_for_object_id_avoiding(
                    object_table,
                    *object_id,
                    |raw| live_ref_raw_is_registered(durable_refs, raw),
                )?;
            (
                u64::from(handle),
                OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
            )
        }
        ObjectValue::Ref(None) => (0, OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED),
        ObjectValue::FuncRef(identity) => {
            let vm_func_ref_addr = durable_refs.resolve_func_identity(*identity).with_context(|| {
                format!(
                    "durable function identity is not registered in this store: module={:#x} function={} layout={}",
                    identity.module_fingerprint,
                    identity.function_index,
                    identity.type_layout_id.get()
                )
            })?;
            (
                u64::try_from(vm_func_ref_addr)
                    .context("durable function reference address does not fit u64")?,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC,
            )
        }
        ObjectValue::ExternRef(identity) => {
            (
                u64::from(durable_refs.resolve_extern_identity(*identity).with_context(|| {
                    format!(
                        "durable external identity is not registered in this store: namespace={} handle={:#x} layout={}",
                        identity.namespace,
                        identity.handle,
                        identity.type_layout_id.get()
                    )
                })?),
                OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
            )
        }
        _ => return ObjectValueAbi::from_object_value(value),
    };
    ObjectValueAbi::from_live_parts(OBJECT_VALUE_ABI_TAG_REF, raw, kind)
}

fn live_ref_raw_is_registered(durable_refs: &DurableReferenceRegistry, raw: u32) -> bool {
    durable_refs.resolve_extern_ref(raw).is_some()
        || durable_refs.resolve_func_ref(raw as usize).is_some()
}

fn ensure_live_ref_raw_does_not_collide_with_transaction_handle(
    state: Option<&TransactionState>,
    object_table: &ObjectTable,
    raw: u32,
    live_ref_kind: &str,
) -> Result<()> {
    ensure!(
        state
            .and_then(|state| state.known_object_id_for_transaction_ref_handle(object_table, raw))
            .is_none(),
        "live {live_ref_kind} reference raw collides with transaction object handle"
    );
    ensure!(
        object_table
            .known_object_id_for_transaction_ref_handle(raw)
            .is_none(),
        "live {live_ref_kind} reference raw collides with transaction object handle"
    );
    Ok(())
}

fn live_object_ref_value_from_ambiguous_raw(
    durable_refs: &DurableReferenceRegistry,
    state: Option<&TransactionState>,
    object_table: &ObjectTable,
    raw: u32,
    include_func_refs: bool,
) -> Result<Option<ObjectValue>> {
    let func = include_func_refs
        .then(|| durable_refs.resolve_func_ref(raw as usize))
        .flatten();
    let extern_ = durable_refs.resolve_extern_ref(raw);
    let bridge_object = object_table.known_object_id_for_live_gc_ref_bridge(raw);
    let handle_object = state
        .and_then(|state| state.known_object_id_for_transaction_ref_handle(object_table, raw))
        .or_else(|| object_table.known_object_id_for_transaction_ref_handle(raw));
    let matches = usize::from(func.is_some())
        + usize::from(extern_.is_some())
        + usize::from(bridge_object.is_some())
        + usize::from(handle_object.is_some());
    ensure!(
        matches <= 1,
        "live reference raw collides with multiple transaction reference namespaces"
    );
    if let Some(identity) = func {
        return Ok(Some(ObjectValue::FuncRef(identity)));
    }
    if let Some(identity) = extern_ {
        return Ok(Some(ObjectValue::ExternRef(identity)));
    }
    if let Some(object_id) = bridge_object.or(handle_object) {
        return Ok(Some(ObjectValue::Ref(Some(object_id))));
    }
    Ok(None)
}

fn live_ref_value_from_raw(
    durable_refs: &mut DurableReferenceRegistry,
    state: Option<&TransactionState>,
    object_table: &mut ObjectTable,
    live_ref_kind: u64,
    raw: u64,
) -> Result<ObjectValue> {
    if live_ref_kind == OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT {
        let handle =
            u32::try_from(raw).context("live persistent object reference does not fit u32")?;
        let object_id = match state {
            Some(state) => state.object_id_for_transaction_ref_handle(object_table, handle)?,
            None => object_table.object_id_for_transaction_ref_handle(handle)?,
        };
        return Ok(ObjectValue::Ref(Some(object_id)));
    }
    if raw == 0 {
        return Ok(ObjectValue::Ref(None));
    }
    match live_ref_kind {
        OBJECT_VALUE_ABI_LIVE_REF_KIND_I31 => {
            return Ok(ObjectValue::I31(ObjectTable::decode_raw_i31_ref(raw)?));
        }
        OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC => {
            let vm_func_ref_addr =
                usize::try_from(raw).context("live function reference does not fit usize")?;
            if let Ok(raw) = u32::try_from(raw) {
                ensure_live_ref_raw_does_not_collide_with_transaction_handle(
                    state,
                    object_table,
                    raw,
                    "function",
                )?;
            }
            let identity = durable_refs.resolve_or_register_live_func_ref(vm_func_ref_addr)?;
            return Ok(ObjectValue::FuncRef(identity));
        }
        OBJECT_VALUE_ABI_LIVE_REF_KIND_GC => {
            if let Ok(gc_ref) = u32::try_from(raw)
                && let Some(value) = live_object_ref_value_from_ambiguous_raw(
                    durable_refs,
                    state,
                    object_table,
                    gc_ref,
                    false,
                )?
            {
                return Ok(value);
            }
            if ObjectTable::is_raw_i31_ref(raw) {
                return Ok(ObjectValue::I31(ObjectTable::decode_raw_i31_ref(raw)?));
            }
        }
        OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN => {
            let raw_gc_ref =
                u32::try_from(raw).context("live external reference does not fit u32")?;
            ensure_live_ref_raw_does_not_collide_with_transaction_handle(
                state,
                object_table,
                raw_gc_ref,
                "external",
            )?;
            let identity = durable_refs.resolve_or_register_live_extern_ref(raw_gc_ref)?;
            return Ok(ObjectValue::ExternRef(identity));
        }
        OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED => {
            if let Ok(gc_ref) = u32::try_from(raw)
                && let Some(value) = live_object_ref_value_from_ambiguous_raw(
                    durable_refs,
                    state,
                    object_table,
                    gc_ref,
                    true,
                )?
            {
                return Ok(value);
            }
            if ObjectTable::is_raw_i31_ref(raw) {
                return Ok(ObjectValue::I31(ObjectTable::decode_raw_i31_ref(raw)?));
            }
            if u32::try_from(raw).is_err() {
                let vm_func_ref_addr =
                    usize::try_from(raw).context("live function reference does not fit usize")?;
                let identity = durable_refs.resolve_or_register_live_func_ref(vm_func_ref_addr)?;
                return Ok(ObjectValue::FuncRef(identity));
            }
        }
        _ => bail!("unknown live ref object value ABI kind"),
    }
    let gc_ref = u32::try_from(raw).context("live GC reference does not fit u32")?;
    Ok(ObjectValue::Ref(Some(
        object_table.object_id_for_live_gc_ref_bridge(gc_ref)?,
    )))
}

fn object_value_abi_bytes(abi: ObjectValueAbi) -> Vec<u8> {
    let (tag, low, high) = abi.as_parts();
    let mut bytes = vec![0; core::mem::size_of::<ObjectValueAbi>()];
    bytes[0..4].copy_from_slice(&tag.to_ne_bytes());
    bytes[8..16].copy_from_slice(&low.to_ne_bytes());
    bytes[16..24].copy_from_slice(&high.to_ne_bytes());
    bytes
}

fn checked_tmemory_effective_address(addr: u64, offset: u64) -> Result<u64> {
    addr.checked_add(offset).with_context(|| {
        format!("out of bounds tmemory access: address {addr} plus offset {offset} overflows")
    })
}

fn flush_pending_tmemory_store(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    let Some((owner, owner_instance_key, memory, addr, len)) = store
        .store_opaque_mut()
        .transaction_state_mut()
        .pending_memory_store()
    else {
        return Ok(());
    };
    let memory_index = TMemoryIndex::from_u32(memory);
    let snapshot =
        collect_tmemory_snapshot(store, owner, owner_instance_key, memory_index, addr, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .flush_tmemory_store_scratch_from_snapshot(&snapshot)?;
    Ok(())
}

fn abort_active_transaction_on_error<T>(store: &mut dyn VMStore, result: &Result<T>) -> Result<()> {
    if result.is_ok() {
        return Ok(());
    }

    #[cfg(feature = "transaction-mvcc")]
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .has_mvcc_terminal_commit()
    {
        return Ok(());
    }

    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    if state.active_transaction().is_some() {
        return state.abort_allocated_objects(object_table);
    }
    Ok(())
}

fn begin_transaction_constructor_boundary(store: &mut dyn VMStore) -> Result<bool> {
    let store = store.store_opaque_mut();
    let region = store.transaction_region_runtime().clone();
    let state = store.transaction_state_mut();
    if state.active_transaction().is_some() {
        return Ok(false);
    }
    state.begin_with_region_runtime(&region)?;
    Ok(true)
}

fn finish_transaction_constructor_boundary<T>(
    store: &mut dyn VMStore,
    began: bool,
    result: &Result<T>,
) -> Result<()> {
    if !began {
        return abort_active_transaction_on_error(store, result);
    }

    if result.is_ok() {
        let commit = {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state
                .begin_terminal_commit_with_object_cleanup(object_table)
                .and_then(|()| state.complete_commit())
        };
        let cleanup = abort_active_transaction_on_error(store, &commit);
        combine_operation_and_cleanup_results(
            commit,
            cleanup,
            "failed to abort transaction after constructor commit failure",
        )
    } else {
        abort_active_transaction_on_error(store, result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TMemoryParticipant {
    owner: InstanceId,
    owner_instance_key: Option<InstanceId>,
    memory_index: u32,
}

impl Ord for TMemoryParticipant {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        (
            self.owner.as_u32(),
            self.owner_instance_key.map(InstanceId::as_u32),
            self.memory_index,
        )
            .cmp(&(
                other.owner.as_u32(),
                other.owner_instance_key.map(InstanceId::as_u32),
                other.memory_index,
            ))
    }
}

impl PartialOrd for TMemoryParticipant {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn collect_tmemory_participants(
    instance: InstanceId,
    records: &[StagedRecord],
) -> BTreeMap<TMemoryParticipant, Vec<(u64, Vec<u8>)>> {
    let mut participants = BTreeMap::new();

    for record in records {
        let StagedRecord::MemoryGranule {
            owner_instance,
            memory_index,
            granule_index,
            bytes,
        } = record
        else {
            continue;
        };
        let participant = TMemoryParticipant {
            owner: owner_instance.unwrap_or(instance),
            owner_instance_key: *owner_instance,
            memory_index: *memory_index,
        };
        participants
            .entry(participant)
            .or_insert_with(Vec::new)
            .push((*granule_index, bytes.clone()));
    }

    participants
}

#[cfg(not(feature = "transaction-mvcc"))]
fn commit_staged_tmemory_records(
    store: &mut dyn VMStore,
    instance: InstanceId,
    records: &[StagedRecord],
    stream_id: u32,
    txid: u32,
) -> Result<Option<PendingCommitLogEntry>> {
    let participants = collect_tmemory_participants(instance, records);
    if participants.is_empty() {
        return Ok(None);
    }

    let mut persistent_undos = Vec::new();
    for (participant, staged) in &participants {
        let memory_index = TMemoryIndex::from_u32(participant.memory_index);
        let backend = {
            let instance_ref = store.instance_mut(participant.owner);
            let instance_ref = instance_ref.as_ref();
            let Some(tmemory) = instance_ref.get_tmemory(memory_index) else {
                bail!("transactional memory operation targeted non-transactional memory");
            };
            tmemory.backend()
        };

        if backend == TMemoryBackend::VMemory {
            continue;
        }

        let instance_ref = store.instance_mut(participant.owner);
        let instance_ref = instance_ref.as_ref();
        let Some(tmemory) = instance_ref.get_tmemory(memory_index) else {
            bail!("transactional memory operation targeted non-transactional memory");
        };
        for (granule_index, bytes) in staged {
            let undo = tmemory.prepare_tmemory_undo_record(
                participant.owner_instance_key.map(InstanceId::as_u32),
                participant.memory_index,
                *granule_index,
                bytes,
            )?;
            persistent_undos.push(undo);
        }
    }

    let mut final_marker = None;
    for undo in &persistent_undos {
        let marker = {
            let state = store.store_opaque_mut().transaction_state_mut();
            state.publish_tmemory_undo_before_in_place_write(stream_id, txid, undo)?
        };
        final_marker = Some(marker);
    }

    for (participant, staged) in &participants {
        let memory_index = TMemoryIndex::from_u32(participant.memory_index);
        {
            let mut instance_ref = store.instance_mut(participant.owner);
            let Some(tmemory) = instance_ref.as_mut().get_tmemory_mut(memory_index) else {
                bail!("transactional memory operation targeted non-transactional memory");
            };
            tmemory.commit_staged_tmemory_granules_direct(staged)?;
        }
    }

    for participant in participants.keys() {
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .remove_staged_tmemory_granules_owned(
                participant.owner_instance_key,
                participant.memory_index,
            )?;
    }

    Ok(final_marker)
}

#[cfg(not(feature = "transaction-mvcc"))]
fn apply_staged_transaction_record(
    store: &mut dyn VMStore,
    instance: InstanceId,
    record: &StagedRecord,
) -> Result<()> {
    match record {
        StagedRecord::MemoryGranule {
            owner_instance,
            memory_index,
            ..
        } => {
            let owner = owner_instance.unwrap_or(instance);
            let memory_index = TMemoryIndex::from_u32(*memory_index);
            let staged = {
                let state = store.store_opaque_mut().transaction_state_mut();
                state.staged_tmemory_granules_owned(Some(owner), memory_index.as_u32())?
            };
            if staged.is_empty() {
                return Ok(());
            }

            {
                let mut instance_ref = store.instance_mut(owner);
                let Some(tmemory) = instance_ref.as_mut().get_tmemory_mut(memory_index) else {
                    bail!("transactional memory operation targeted non-transactional memory");
                };
                let staged = staged
                    .iter()
                    .map(|(_, granule_index, bytes)| {
                        Ok((
                            u64::try_from(*granule_index)
                                .context("tmemory granule index does not fit durable log")?,
                            bytes.clone(),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                tmemory.commit_staged_tmemory_granules_direct(&staged)?;
            }

            store
                .store_opaque_mut()
                .transaction_state_mut()
                .remove_staged_tmemory_granules_owned(Some(owner), memory_index.as_u32())?;
        }
        StagedRecord::Global {
            owner_instance,
            global_index,
            value,
        } => {
            let owner = owner_instance.unwrap_or(instance);
            let global_index = TGlobalIndex::from_u32(*global_index);
            let (_, _, wasm_ty) = transaction_global(store, owner, global_index.as_u32())?;
            let mut global = global_definition_ptr(store, owner, global_index)?;
            let global = unsafe { global.as_mut() };
            write_transaction_global_snapshot(store.store_opaque_mut(), global, wasm_ty, *value)?;
        }
        StagedRecord::MemorySize {
            owner_instance,
            memory_index,
            new_pages,
        } => {
            let owner = owner_instance.unwrap_or(instance);
            grow_tmemory_to_pages(store, owner, *memory_index, *new_pages)?;
        }
        StagedRecord::TableSize {
            owner_instance,
            table_index,
            new_elements,
        } => {
            let owner = owner_instance.unwrap_or(instance);
            grow_defined_table_to(store, owner, *table_index, *new_elements)?;
        }
        StagedRecord::TableElement {
            owner_instance,
            table_index,
            element_index,
            value,
        } => {
            let owner = owner_instance.unwrap_or(instance);
            write_table_element_snapshot(store, owner, *table_index, *element_index, *value)?;
        }
    }
    Ok(())
}

fn ensure_active_transaction(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    #[cfg(feature = "transaction-mvcc")]
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .has_mvcc_terminal_commit()
    {
        transaction_commit_mvcc_impl(store, instance)?;
        let store = store.store_opaque_mut();
        let region = store.transaction_region_runtime().clone();
        store
            .transaction_state_mut()
            .begin_with_region_runtime(&region)?;
    }
    #[cfg(not(feature = "transaction-mvcc"))]
    let _ = instance;

    ensure!(
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .active_transaction()
            .is_some(),
        "transaction operation requires an active transaction"
    );
    Ok(())
}

fn resolve_defined_tmemory_index(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
) -> Result<TMemoryIndex> {
    let defined = DefinedMemoryIndex::from_u32(memory);
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let module = instance_ref.env_module();
    let defined = module
        .defined_tmemory_index_from_runtime(defined)
        .context("transactional memory builtin received an ordinary memory slot")?;
    let memory_index = module.tmemory_index(defined);
    ensure!(
        instance_ref.get_tmemory(memory_index).is_some(),
        "transactional memory operation targeted non-transactional memory"
    );
    Ok(memory_index)
}

fn tmemory_transaction_owner_key(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: TMemoryIndex,
) -> Result<Option<InstanceId>> {
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(memory_index)
        .context("transactional memory operation targeted non-transactional memory")?;
    Ok(match tmemory.backend() {
        TMemoryBackend::FileBackedMemory => None,
        TMemoryBackend::VMemory | TMemoryBackend::DaxPmem => Some(instance),
    })
}

fn grow_tmemory_to_pages(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    new_pages: u64,
) -> Result<()> {
    let runtime = store.store_opaque().transaction_region_runtime().clone();
    runtime.with_shared_file_backed_tmemory_commit_write_lock(|| {
        let memory = TMemoryIndex::from_u32(memory);
        let mut instance_ref = store.instance_mut(instance);
        let Some(tmemory) = instance_ref.as_mut().get_tmemory_mut(memory) else {
            bail!("transactional memory operation targeted non-transactional memory");
        };
        let current_pages =
            u64::try_from(tmemory.byte_len() / crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE)
                .context("tmemory current size overflow")?;
        if new_pages <= current_pages {
            return Ok(());
        }
        tmemory
            .grow_to_pages(new_pages)
            .context("transactional memory grow failed during commit")?;
        runtime.record_file_backed_tmemory_pages(new_pages)?;
        Ok(())
    })
}

fn defined_table_size(store: &mut dyn VMStore, instance: InstanceId, table: u32) -> Result<usize> {
    let table = DefinedTableIndex::from_u32(table);
    let mut instance_ref = store.instance_mut(instance);
    Ok(instance_ref.as_mut().get_defined_table(table).size())
}

fn grow_defined_table_to(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    new_size: u64,
) -> Result<()> {
    let current_size = u64::try_from(defined_table_size(store, instance, table)?)
        .context("defined table size overflow")?;
    if new_size <= current_size {
        return Ok(());
    }

    let delta = new_size - current_size;
    let table = DefinedTableIndex::from_u32(table);
    let (mut limiter, store) = store.resource_limiter_and_store_opaque();
    let limiter = limiter.as_mut();
    block_on!(store, async |store, _| unsafe {
        ensure!(
            store
                .instance_mut(instance)
                .defined_table_grow(table, limiter, delta)
                .await?
                .is_some(),
            "transactional table grow failed during commit"
        );
        Ok(())
    })?
}

fn ensure_defined_table_index_in_bounds(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
) -> Result<()> {
    let index = usize::try_from(index).map_err(|_| Trap::TableOutOfBounds)?;
    let size = defined_table_size(store, instance, table)?;
    if index >= size {
        bail!(Trap::TableOutOfBounds);
    }
    Ok(())
}

fn transaction_table_size_for_bounds(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<u64> {
    if store
        .store_opaque_mut()
        .transaction_state_mut()
        .active_transaction()
        .is_none()
    {
        return u64::try_from(defined_table_size(store, instance, table)?)
            .context("defined table size does not fit u64");
    }
    visible_ttable_size(store, instance, table)
}

fn visible_ttable_size(store: &mut dyn VMStore, instance: InstanceId, table: u32) -> Result<u64> {
    let staged = store
        .store_opaque_mut()
        .transaction_state_mut()
        .staged_table_size_owned(Some(instance), table);
    if let Some(size) = staged {
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .acquire_table_size_read_owned(Some(instance), table, 0)?;
        return Ok(size);
    }
    snapshot_visible_ttable_size(store, instance, table)
}

fn snapshot_visible_ttable_size(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<u64> {
    let visibility = {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_table_size_read_owned(Some(instance), table, 0)?;
        state.active_visibility_read_context()?
    };
    visibility.read_table_size(
        GranuleId::TTableSize {
            instance: Some(instance.as_u32()),
            table_index: table,
        },
        || {
            u64::try_from(defined_table_size(store, instance, table)?)
                .context("defined table size does not fit u64")
        },
    )
}

fn collect_current_table_granule(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    granule_index: u64,
    visible_size: u64,
) -> Result<TableGranuleSnapshot> {
    let granule_start = granule_index
        .checked_mul(TableGranuleSnapshot::ELEMENT_CAPACITY)
        .context("ttable granule start overflow")?;
    let granule_end = granule_start
        .checked_add(TableGranuleSnapshot::ELEMENT_CAPACITY)
        .context("ttable granule end overflow")?
        .min(visible_size);
    let table_index = DefinedTableIndex::from_u32(table);
    let mut store = AutoAssertNoGc::new(store.store_opaque_mut());
    let (_gc_store, registry, instance_ref) =
        store.optional_gc_store_and_registry_and_instance_mut(instance);
    let table_ref = instance_ref.get_defined_table_with_lazy_init(
        registry,
        table_index,
        granule_start..granule_end,
    );
    let mut elements = Vec::with_capacity(usize::try_from(granule_end - granule_start).unwrap());
    for element_index in granule_start..granule_end {
        elements.push(match table_ref.element_type() {
            TableElementType::Func => TableElementSnapshot::FuncRef(
                table_ref
                    .get_func(element_index)?
                    .map_or(0, |ptr| ptr.as_ptr().addr()),
            ),
            TableElementType::GcRef => TableElementSnapshot::GcRef(
                table_ref
                    .get_gc_ref(element_index)?
                    .map_or(0, VMGcRef::as_raw_u32),
            ),
            TableElementType::Cont => {
                bail!("transactional contref table is not implemented yet")
            }
        });
    }
    TableGranuleSnapshot::new(elements)
}

fn ensure_transaction_table_index_in_bounds(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
) -> Result<()> {
    let size = transaction_table_size_for_bounds(store, instance, table)?;
    if index >= size {
        bail!(Trap::TableOutOfBounds);
    }
    Ok(())
}

fn ensure_transaction_table_range_in_bounds(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    let size = transaction_table_size_for_bounds(store, instance, table)?;
    let end = start.checked_add(len).ok_or(Trap::TableOutOfBounds)?;
    if start > size || end > size {
        bail!(Trap::TableOutOfBounds);
    }
    Ok(())
}

fn collect_defined_tmemory_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    addr: u64,
    len: usize,
) -> Result<(TMemoryIndex, TMemoryAccessSnapshot)> {
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let owner_instance_key = tmemory_transaction_owner_key(store, instance, memory_index)?;
    let snapshot =
        collect_tmemory_snapshot(store, instance, owner_instance_key, memory_index, addr, len)?;
    Ok((memory_index, snapshot))
}

fn current_tmemory_pages(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: TMemoryIndex,
) -> Result<u64> {
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(memory_index)
        .context("transactional memory operation targeted non-transactional memory")?;
    u64::try_from(tmemory.byte_len() / crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE)
        .context("tmemory size overflow")
}

fn current_tmemory_granule_version(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: TMemoryIndex,
    granule_index: u64,
) -> Result<u64> {
    let granule_index =
        usize::try_from(granule_index).context("tmemory granule index does not fit host usize")?;
    let granule_start = granule_index
        .checked_mul(crate::runtime::transaction::TMEMORY_GRANULE_SIZE)
        .context("tmemory granule start overflow")?;
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(memory_index)
        .context("transactional memory operation targeted non-transactional memory")?;
    if granule_start >= tmemory.byte_len() {
        return Ok(0);
    }
    tmemory.granule_version(granule_index)
}

fn snapshot_visible_tmemory_pages(
    store: &mut dyn VMStore,
    instance: InstanceId,
    owner_instance_key: Option<InstanceId>,
    memory_index: TMemoryIndex,
) -> Result<u64> {
    let (visibility, granule) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_memory_size_read_owned(owner_instance_key, memory_index.as_u32())?;
        (
            state.active_visibility_read_context()?,
            GranuleId::TMemorySize {
                instance: owner_instance_key.map(InstanceId::as_u32),
                memory_index: memory_index.as_u32(),
            },
        )
    };
    visibility.read_memory_size(granule, || {
        current_tmemory_pages(store, instance, memory_index)
    })
}

fn visible_tmemory_pages(
    store: &mut dyn VMStore,
    instance: InstanceId,
    owner_instance_key: Option<InstanceId>,
    memory_index: TMemoryIndex,
) -> Result<u64> {
    let staged = store
        .store_opaque_mut()
        .transaction_state_mut()
        .staged_memory_size_owned(owner_instance_key, memory_index.as_u32());
    match staged {
        Some(pages) => {
            store
                .store_opaque_mut()
                .transaction_state_mut()
                .acquire_memory_size_read_owned(owner_instance_key, memory_index.as_u32())?;
            Ok(pages)
        }
        None => snapshot_visible_tmemory_pages(store, instance, owner_instance_key, memory_index),
    }
}

fn collect_tmemory_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    _owner_instance_key: Option<InstanceId>,
    memory_index: TMemoryIndex,
    addr: u64,
    len: usize,
) -> Result<TMemoryAccessSnapshot> {
    #[cfg(not(feature = "transaction-mvcc"))]
    {
        // Register the observed versions before copying the bytes. A writer
        // can otherwise publish between the copy and registration, making an
        // old snapshot appear to have the writer's new shared version.
        let versions = {
            let instance_ref = store.instance_mut(instance);
            let instance_ref = instance_ref.as_ref();
            let tmemory = instance_ref
                .get_tmemory(memory_index)
                .context("transactional memory operation targeted non-transactional memory")?;
            collect_tmemory_access_versions(tmemory, addr, len)?
        };
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .register_tmemory_access_versions(
                _owner_instance_key,
                memory_index.as_u32(),
                &versions,
            )?;
        let instance_ref = store.instance_mut(instance);
        let instance_ref = instance_ref.as_ref();
        let tmemory = instance_ref
            .get_tmemory(memory_index)
            .context("transactional memory operation targeted non-transactional memory")?;
        return collect_tmemory_access_snapshot(tmemory, addr, len);
    }

    #[cfg(feature = "transaction-mvcc")]
    {
        let snapshot_pages =
            snapshot_visible_tmemory_pages(store, instance, _owner_instance_key, memory_index)?;
        let access_pages = store
            .store_opaque_mut()
            .transaction_state_mut()
            .staged_memory_size_owned(_owner_instance_key, memory_index.as_u32())
            .unwrap_or(snapshot_pages);
        let page_size = crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE;
        let snapshot_byte_len = usize::try_from(snapshot_pages)
            .ok()
            .and_then(|pages| pages.checked_mul(page_size))
            .context("snapshot-visible tmemory size overflow")?;
        let access_byte_len = usize::try_from(access_pages)
            .ok()
            .and_then(|pages| pages.checked_mul(page_size))
            .context("transactional tmemory size overflow")?;
        let range_start =
            usize::try_from(addr).context("tmemory address does not fit host usize")?;
        let range_end = range_start
            .checked_add(len)
            .context("tmemory address overflow")?;
        ensure!(
            range_end <= access_byte_len,
            "out of bounds tmemory access: range {range_start}..{range_end} exceeds backing length {access_byte_len}"
        );
        let range = range_start..range_end;
        let visibility = store
            .store_opaque_mut()
            .transaction_state_mut()
            .active_visibility_read_context()?;
        let mut granules = Vec::new();
        let mut bytes = Vec::with_capacity(len);
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / crate::runtime::transaction::TMEMORY_GRANULE_SIZE;
            let granule_start = granule_index
                .checked_mul(crate::runtime::transaction::TMEMORY_GRANULE_SIZE)
                .context("tmemory granule start overflow")?;
            let granule_end = granule_start
                .checked_add(crate::runtime::transaction::TMEMORY_GRANULE_SIZE)
                .context("tmemory granule end overflow")?
                .min(access_byte_len);
            let granule_range = granule_start..granule_end;
            let visible_bytes = if granule_start < snapshot_byte_len {
                let visible_end = granule_end.min(snapshot_byte_len);
                let current_len = visible_end - granule_start;
                let granule = GranuleId::TMemory {
                    instance: _owner_instance_key.map(InstanceId::as_u32),
                    memory_index: memory_index.as_u32(),
                    granule_index: u64::try_from(granule_index)
                        .context("tmemory granule index does not fit u64")?,
                };
                visibility.read_memory(granule, || {
                    let instance_ref = store.instance_mut(instance);
                    let instance_ref = instance_ref.as_ref();
                    let tmemory = instance_ref.get_tmemory(memory_index).context(
                        "transactional memory operation targeted non-transactional memory",
                    )?;
                    let snapshot = collect_tmemory_access_snapshot(
                        tmemory,
                        u64::try_from(granule_start)
                            .context("tmemory granule address does not fit u64")?,
                        current_len,
                    )?;
                    Ok(snapshot
                        .into_granules()
                        .into_iter()
                        .next()
                        .context("current tmemory snapshot is missing its granule")?
                        .into_bytes())
                })?
            } else {
                vec![0; granule_range.len()]
            };
            ensure!(
                visible_bytes.len() == granule_range.len(),
                "visible tmemory granule length mismatch"
            );
            let overlap_start = current - granule_start;
            let overlap_end = (range.end.min(granule_end)) - granule_start;
            bytes.extend_from_slice(&visible_bytes[overlap_start..overlap_end]);
            let current_version = current_tmemory_granule_version(
                store,
                instance,
                memory_index,
                u64::try_from(granule_index).context("tmemory granule index does not fit u64")?,
            )?;
            granules.push(TMemoryGranuleSnapshot::new(
                granule_index,
                granule_range,
                current_version,
                visible_bytes,
            )?);
            current = range.end.min(granule_end);
        }

        TMemoryAccessSnapshot::new(addr, bytes, access_byte_len, granules)
    }
}

fn current_granule_version(
    store: &mut dyn VMStore,
    instance: InstanceId,
    granule: GranuleId,
) -> Result<u64> {
    match granule {
        GranuleId::TMemory {
            instance: owner_instance,
            memory_index,
            granule_index,
        } => {
            let owner = owner_instance.map(InstanceId::from_u32).unwrap_or(instance);
            let memory_index = TMemoryIndex::from_u32(memory_index);
            current_tmemory_granule_version(store, owner, memory_index, granule_index)
        }
        GranuleId::Object { .. } => {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state.current_object_version_for_granule(granule, &*object_table)
        }
        GranuleId::TMemorySize { .. }
        | GranuleId::TGlobal { .. }
        | GranuleId::TTable { .. }
        | GranuleId::TTableSize { .. } => Ok(store
            .store_opaque_mut()
            .transaction_state_mut()
            .versioned_granule_version(granule)),
    }
}

/// A helper structure to represent the return value of a memory or table growth
/// call.
///
/// This represents a byte or element-based count of the size of an item on the
/// host. For example a memory is how many bytes large the memory is, or a table
/// is how many elements large it is. It's assumed that the value here is never
/// -1 or -2 as that would mean the entire host address space is allocated which
/// is not possible.
struct AllocationSize(usize);

/// Special implementation for growth-related libcalls.
///
/// Here the optional return value means:
///
/// * `Some(val)` - the growth succeeded and the previous size of the item was
///   `val`.
/// * `None` - the growth failed.
///
/// The failure case returns -1 (or `usize::MAX` as an unsigned integer) and the
/// successful case returns the `val` itself. Note that -2 (`usize::MAX - 1`
/// when unsigned) is unwind as a sentinel to indicate an unwind as no valid
/// allocation can be that large.
unsafe impl HostResultHasUnwindSentinel for Option<AllocationSize> {
    type Abi = *mut u8;
    const SENTINEL: *mut u8 = (usize::MAX - 1) as *mut u8;

    fn into_abi(self) -> *mut u8 {
        match self {
            Some(size) => {
                debug_assert!(size.0 < (usize::MAX - 1));
                size.0 as *mut u8
            }
            None => usize::MAX as *mut u8,
        }
    }
}

/// Implementation of `table.grow`.
unsafe fn table_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    defined_table_index: u32,
    delta: u64,
) -> Result<Option<AllocationSize>> {
    let defined_table_index = DefinedTableIndex::from_u32(defined_table_index);
    let (mut limiter, store) = store.resource_limiter_and_store_opaque();
    let limiter = limiter.as_mut();
    block_on!(store, async |store, _| unsafe {
        let result = store
            .instance_mut(instance)
            .defined_table_grow(defined_table_index, limiter, delta)
            .await?
            .map(AllocationSize);
        Ok(result)
    })?
}

fn passive_elem_segment_len(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> usize {
    let elem_index = PassiveElemIndex::from_u32(elem_index);
    store
        .instance_mut(instance)
        .passive_element_segment(elem_index)
        .len()
}

fn passive_elem_segment_base(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> *mut u8 {
    let elem_index = PassiveElemIndex::from_u32(elem_index);
    store
        .instance_mut(instance)
        .passive_element_segment(elem_index)
        .as_mut_ptr()
        .cast()
}

// Implementation of `elem.drop`.
fn passive_elem_segment_drop(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> Result<()> {
    let elem_index = PassiveElemIndex::from_u32(elem_index);
    let (gc_store, instance) = store.optional_gc_store_and_instance_mut(instance);
    instance.passive_elem_drop(gc_store, elem_index)?;
    Ok(())
}

fn passive_telem_segment_len(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> usize {
    let elem_index = PassiveTElemIndex::from_u32(elem_index);
    store
        .instance_mut(instance)
        .passive_telement_segment(elem_index)
        .len()
}

fn passive_telem_segment_base(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> *mut u8 {
    let elem_index = PassiveTElemIndex::from_u32(elem_index);
    store
        .instance_mut(instance)
        .passive_telement_segment(elem_index)
        .as_mut_ptr()
        .cast()
}

fn passive_telem_segment_drop(
    store: &mut dyn VMStore,
    instance: InstanceId,
    elem_index: u32,
) -> Result<()> {
    let elem_index = PassiveTElemIndex::from_u32(elem_index);
    let (gc_store, instance) = store.optional_gc_store_and_instance_mut(instance);
    instance.passive_telem_drop(gc_store, elem_index)?;
    Ok(())
}

// Implementation of `memory.copy`.
unsafe fn memory_copy(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    dst: *mut u8,
    src: *mut u8,
    len: usize,
) {
    let src = src.cast_const();
    // FIXME(#4203): this is known to not be sound in the presence of shared
    // memories.
    unsafe { src.copy_to(dst, len) }
}

unsafe fn memory_fill(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    dst: *mut u8,
    val: u32,
    len: usize,
) {
    // FIXME(#4203): this is known to not be sound in the presence of shared
    // memories.
    unsafe {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the libcall intentionally takes the raw 32-bit value, \
                      and semantically that's intentionally truncated"
        )]
        dst.write_bytes(val as u8, len);
    }
}

// Implementation of `ref.func`.
fn ref_func(store: &mut dyn VMStore, instance: InstanceId, func_index: u32) -> NonNull<u8> {
    let (instance, registry) = store.instance_and_module_registry_mut(instance);
    instance
        .get_func_ref(registry, FuncIndex::from_u32(func_index))
        .expect("ref_func: funcref should always be available for given func index")
        .cast()
}

// Returns a table entry after lazily initializing it.
fn table_get_lazy_init_func_ref(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table_index: u32,
    index: u64,
) -> *mut u8 {
    let table_index = TableIndex::from_u32(table_index);
    let (instance, registry) = store.instance_and_module_registry_mut(instance);
    let table = instance.get_table_with_lazy_init(registry, table_index, core::iter::once(index));
    let elem = table
        .get_func(index)
        .expect("table access already bounds-checked");

    match elem {
        Some(ptr) => ptr.as_ptr().cast(),
        None => core::ptr::null_mut(),
    }
}

/// Drop a GC reference.
#[cfg(feature = "gc-drc")]
fn drop_gc_ref(store: &mut dyn VMStore, _instance: InstanceId, gc_ref: u32) {
    log::trace!("libcalls::drop_gc_ref({gc_ref:#x})");
    let gc_ref = VMGcRef::from_raw_u32(gc_ref).expect("non-null VMGcRef");
    store
        .store_opaque_mut()
        .unwrap_gc_store_mut()
        .drop_gc_ref(gc_ref);
}

/// Force a DRC GC cycle.
#[cfg(feature = "gc-drc")]
fn force_gc(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    let store = store.store_opaque_mut();
    block_on!(store, async |store, asyncness| {
        store.gc(None, None, None, asyncness).await?;
        Ok::<(), Error>(())
    })??;
    Ok(())
}

/// Grow the GC heap.
#[cfg(feature = "gc-null")]
fn grow_gc_heap(store: &mut dyn VMStore, _instance: InstanceId, bytes_needed: u64) -> Result<()> {
    let orig_len = u64::try_from(
        store
            .require_gc_store()?
            .gc_heap
            .vmmemory()
            .current_length(),
    )
    .unwrap();

    let (mut limiter, store) = store.resource_limiter_and_store_opaque();
    block_on!(store, async |store, asyncness| {
        // We error below if there's still not enough space; swallow
        // any growth failures here.
        let _ = store
            .grow_gc_heap(limiter.as_mut(), bytes_needed, asyncness)
            .await;
    })?;

    // JIT code relies on the memory having grown by `bytes_needed` bytes if
    // this libcall returns successfully, so trap if we didn't grow that much.
    let new_len = u64::try_from(
        store
            .require_gc_store()?
            .gc_heap
            .vmmemory()
            .current_length(),
    )
    .unwrap();
    if orig_len
        .checked_add(bytes_needed)
        .is_none_or(|expected_len| new_len < expected_len)
    {
        return Err(crate::Trap::AllocationTooLarge.into());
    }

    Ok(())
}

/// Allocate a raw, unininitialized GC object for Wasm code.
///
/// The Wasm code is responsible for initializing the object.
#[cfg(any(feature = "gc-drc", feature = "gc-copying"))]
fn gc_alloc_raw(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    kind_and_reserved: u32,
    shared_type_index: u32,
    size: u32,
    align: u32,
) -> Result<core::num::NonZeroU32> {
    use crate::vm::VMGcHeader;
    use core::alloc::Layout;
    use wasmtime_environ::{VMGcKind, VMSharedTypeIndex};

    let kind = VMGcKind::from_high_bits_of_u32(kind_and_reserved);
    log::trace!("gc_alloc_raw(kind={kind:?}, size={size}, align={align})");

    let shared_type_index = VMSharedTypeIndex::from_u32(shared_type_index);
    let mut header = VMGcHeader::from_kind_and_index(kind, shared_type_index);
    header.set_reserved_u26(kind_and_reserved & VMGcKind::UNUSED_MASK);

    let size = usize::try_from(size).unwrap();
    let align = usize::try_from(align).unwrap();
    assert!(align.is_power_of_two());
    let layout = Layout::from_size_align(size, align).map_err(|e| {
        let err = Error::from(crate::Trap::AllocationTooLarge);
        err.context(e)
    })?;

    // Fast path: when the GC store already exists, try to allocate directly to
    // skip the async/fiber machinery.
    let opaque = store.store_opaque_mut();
    if let Some(gc_store) = opaque.try_gc_store_mut() {
        if let Ok(gc_ref) = gc_store.alloc_raw(header, layout)? {
            let raw = gc_store.expose_gc_ref_to_wasm(gc_ref)?;
            return Ok(raw);
        }
    }

    let (mut limiter, store) = store.resource_limiter_and_store_opaque();
    block_on!(store, async |store, asyncness| {
        let gc_ref = store
            .retry_after_gc_async(limiter.as_mut(), (), asyncness, |store, ()| {
                store
                    .unwrap_gc_store_mut()
                    .alloc_raw(header, layout)?
                    .map_err(|bytes_needed| crate::GcHeapOutOfMemory::new((), bytes_needed).into())
            })
            .await?;

        store.unwrap_gc_store_mut().expose_gc_ref_to_wasm(gc_ref)
    })?
}

// Intern a `funcref` into the GC heap, returning its `FuncRefTableId`.
//
// This libcall may not GC.
#[cfg(feature = "gc")]
unsafe fn intern_func_ref_for_gc_heap(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    func_ref: *mut u8,
) -> Result<u32> {
    use crate::runtime::vm::vmcontext::VMFuncRef;
    use crate::{store::AutoAssertNoGc, vm::SendSyncPtr};
    use core::ptr::NonNull;

    let mut store = AutoAssertNoGc::new(store.store_opaque_mut());

    let func_ref = func_ref.cast::<VMFuncRef>();
    let func_ref = NonNull::new(func_ref).map(SendSyncPtr::new);

    let func_ref_id = unsafe {
        store
            .require_gc_store_mut()?
            .func_ref_table
            .intern(func_ref)
    };
    Ok(func_ref_id.into_raw())
}

// Get the raw `VMFuncRef` pointer associated with a `FuncRefTableId` from an
// earlier `intern_func_ref_for_gc_heap` call.
//
// This libcall may not GC.
#[cfg(feature = "gc")]
fn get_interned_func_ref(
    store: &mut dyn VMStore,
    instance: InstanceId,
    func_ref_id: u32,
    module_interned_type_index: u32,
) -> Result<*mut u8> {
    use super::FuncRefTableId;
    use crate::store::AutoAssertNoGc;
    use wasmtime_environ::{ModuleInternedTypeIndex, packed_option::ReservedValue};

    let store = AutoAssertNoGc::new(store.store_opaque_mut());

    let func_ref_id = FuncRefTableId::from_raw(func_ref_id);
    let module_interned_type_index = ModuleInternedTypeIndex::from_bits(module_interned_type_index);

    let func_ref = if module_interned_type_index.is_reserved_value() {
        store
            .unwrap_gc_store()
            .func_ref_table
            .get_untyped(func_ref_id)?
    } else {
        let types = store.engine().signatures();
        let engine_ty = store
            .instance(instance)
            .engine_type_index(module_interned_type_index);
        store
            .unwrap_gc_store()
            .func_ref_table
            .get_typed(types, func_ref_id, engine_ty)?
    };

    Ok(func_ref.map_or(core::ptr::null_mut(), |f| f.as_ptr().cast()))
}

#[cfg(feature = "gc")]
fn is_subtype(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    actual_engine_type: u32,
    expected_engine_type: u32,
) -> u32 {
    use wasmtime_environ::VMSharedTypeIndex;

    let actual = VMSharedTypeIndex::from_u32(actual_engine_type);
    let expected = VMSharedTypeIndex::from_u32(expected_engine_type);

    let is_subtype: bool = store.engine().signatures().is_subtype(actual, expected);

    log::trace!("is_subtype(actual={actual:?}, expected={expected:?}) -> {is_subtype}",);
    is_subtype as u32
}

// Implementation of `memory.atomic.notify` for locally defined memories.
#[cfg(feature = "threads")]
fn memory_atomic_notify(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: u32,
    addr_index: u64,
    count: u32,
) -> Result<u32, Trap> {
    let memory = DefinedMemoryIndex::from_u32(memory_index);
    store
        .instance_mut(instance)
        .get_defined_memory_mut(memory)
        .atomic_notify(addr_index, count)
}

// Implementation of `memory.atomic.wait32` for locally defined memories.
#[cfg(feature = "threads")]
fn memory_atomic_wait32(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: u32,
    addr_index: u64,
    expected: u32,
    timeout: u64,
) -> Result<u32, Trap> {
    let timeout = (timeout as i64 >= 0).then(|| Duration::from_nanos(timeout));
    let memory = DefinedMemoryIndex::from_u32(memory_index);
    Ok(store
        .instance_mut(instance)
        .get_defined_memory_mut(memory)
        .atomic_wait32(addr_index, expected, timeout)? as u32)
}

// Implementation of `memory.atomic.wait64` for locally defined memories.
#[cfg(feature = "threads")]
fn memory_atomic_wait64(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: u32,
    addr_index: u64,
    expected: u64,
    timeout: u64,
) -> Result<u32, Trap> {
    let timeout = (timeout as i64 >= 0).then(|| Duration::from_nanos(timeout));
    let memory = DefinedMemoryIndex::from_u32(memory_index);
    Ok(store
        .instance_mut(instance)
        .get_defined_memory_mut(memory)
        .atomic_wait64(addr_index, expected, timeout)? as u32)
}

// Hook for when an instance runs out of fuel.
fn out_of_gas(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    block_on!(store, async |store, _| {
        if !store.refuel() {
            return Err(Trap::OutOfFuel.into());
        }
        #[cfg(feature = "async")]
        if store.fuel_yield_interval.is_some() {
            store.yield_now().await;
        }
        Ok(())
    })?
}

// Hook for when an instance observes that the epoch has changed.
#[cfg(target_has_atomic = "64")]
fn new_epoch(store: &mut dyn VMStore, _instance: InstanceId) -> Result<NextEpoch> {
    use crate::UpdateDeadline;

    #[cfg(feature = "debug")]
    {
        store.block_on_debug_handler(crate::DebugEvent::EpochYield)?;
    }

    let update_deadline = store.new_epoch_updated_deadline()?;
    block_on!(store, async move |store, asyncness| {
        #[cfg(not(feature = "async"))]
        let _ = asyncness;

        let delta = match update_deadline {
            UpdateDeadline::Interrupt => return Err(Trap::Interrupt.into()),
            UpdateDeadline::Continue(delta) => delta,

            // Note that custom errors are used here to avoid tripping up on the
            // `block_on!` message that otherwise assumes
            // async-configuration-after-the-fact.
            #[cfg(feature = "async")]
            UpdateDeadline::Yield(delta) => {
                if asyncness != Asyncness::Yes {
                    bail!(
                        "cannot use `UpdateDeadline::Yield` without using \
                         an async wasm entrypoint",
                    );
                }
                store.yield_now().await;
                delta
            }
            #[cfg(feature = "async")]
            UpdateDeadline::YieldCustom(delta, future) => {
                if asyncness != Asyncness::Yes {
                    bail!(
                        "cannot use `UpdateDeadline::YieldCustom` without using \
                         an async wasm entrypoint",
                    );
                }
                future.await;
                delta
            }
        };

        // Set a new deadline and return the new epoch deadline so
        // the Wasm code doesn't have to reload it.
        store.set_epoch_deadline(delta);
        Ok(NextEpoch(store.get_epoch_deadline()))
    })?
}

struct NextEpoch(u64);

unsafe impl HostResultHasUnwindSentinel for NextEpoch {
    type Abi = u64;
    const SENTINEL: u64 = u64::MAX;
    fn into_abi(self) -> u64 {
        self.0
    }
}

// Hook for validating malloc using wmemcheck_state.
#[cfg(feature = "wmemcheck")]
fn check_malloc(store: &mut dyn VMStore, instance: InstanceId, addr: u32, len: u32) -> Result<()> {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        let result = wmemcheck_state.malloc(addr as usize, len as usize);
        wmemcheck_state.memcheck_on();
        match result {
            Ok(()) => {}
            Err(DoubleMalloc { addr, len }) => {
                bail!("Double malloc at addr {:#x} of size {}", addr, len)
            }
            Err(OutOfBounds { addr, len }) => {
                bail!("Malloc out of bounds at addr {:#x} of size {}", addr, len);
            }
            _ => {
                panic!("unreachable")
            }
        }
    }
    Ok(())
}

// Hook for validating free using wmemcheck_state.
#[cfg(feature = "wmemcheck")]
fn check_free(store: &mut dyn VMStore, instance: InstanceId, addr: u32) -> Result<()> {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        let result = wmemcheck_state.free(addr as usize);
        wmemcheck_state.memcheck_on();
        match result {
            Ok(()) => {}
            Err(InvalidFree { addr }) => {
                bail!("Invalid free at addr {:#x}", addr)
            }
            _ => {
                panic!("unreachable")
            }
        }
    }
    Ok(())
}

// Hook for validating load using wmemcheck_state.
#[cfg(feature = "wmemcheck")]
fn check_load(
    store: &mut dyn VMStore,
    instance: InstanceId,
    num_bytes: u32,
    addr: u32,
    offset: u32,
) -> Result<()> {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        let result = wmemcheck_state.read(addr as usize + offset as usize, num_bytes as usize);
        match result {
            Ok(()) => {}
            Err(InvalidRead { addr, len }) => {
                bail!("Invalid load at addr {:#x} of size {}", addr, len);
            }
            Err(OutOfBounds { addr, len }) => {
                bail!("Load out of bounds at addr {:#x} of size {}", addr, len);
            }
            _ => {
                panic!("unreachable")
            }
        }
    }
    Ok(())
}

// Hook for validating store using wmemcheck_state.
#[cfg(feature = "wmemcheck")]
fn check_store(
    store: &mut dyn VMStore,
    instance: InstanceId,
    num_bytes: u32,
    addr: u32,
    offset: u32,
) -> Result<()> {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        let result = wmemcheck_state.write(addr as usize + offset as usize, num_bytes as usize);
        match result {
            Ok(()) => {}
            Err(InvalidWrite { addr, len }) => {
                bail!("Invalid store at addr {:#x} of size {}", addr, len)
            }
            Err(OutOfBounds { addr, len }) => {
                bail!("Store out of bounds at addr {:#x} of size {}", addr, len)
            }
            _ => {
                panic!("unreachable")
            }
        }
    }
    Ok(())
}

// Hook for turning wmemcheck load/store validation off when entering a malloc function.
#[cfg(feature = "wmemcheck")]
fn malloc_start(store: &mut dyn VMStore, instance: InstanceId) {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        wmemcheck_state.memcheck_off();
    }
}

// Hook for turning wmemcheck load/store validation off when entering a free function.
#[cfg(feature = "wmemcheck")]
fn free_start(store: &mut dyn VMStore, instance: InstanceId) {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        wmemcheck_state.memcheck_off();
    }
}

// Hook for tracking wasm stack updates using wmemcheck_state.
#[cfg(feature = "wmemcheck")]
fn update_stack_pointer(_store: &mut dyn VMStore, _instance: InstanceId, _value: u32) {
    // TODO: stack-tracing has yet to be finalized. All memory below
    // the address of the top of the stack is marked as valid for
    // loads and stores.
    // if let Some(wmemcheck_state) = &mut instance.wmemcheck_state {
    //     instance.wmemcheck_state.update_stack_pointer(value as usize);
    // }
}

// Hook updating wmemcheck_state memory state vector every time memory.grow is called.
#[cfg(feature = "wmemcheck")]
fn update_mem_size(store: &mut dyn VMStore, instance: InstanceId, num_pages: u32) {
    let instance = store.instance_mut(instance);
    if let Some(wmemcheck_state) = instance.wmemcheck_state_mut() {
        const KIB: usize = 1024;
        let num_bytes = num_pages as usize * 64 * KIB;
        wmemcheck_state.update_mem_size(num_bytes);
    }
}

fn floor_f32(_store: &mut dyn VMStore, _instance: InstanceId, val: f32) -> f32 {
    val.wasm_floor()
}

fn floor_f64(_store: &mut dyn VMStore, _instance: InstanceId, val: f64) -> f64 {
    val.wasm_floor()
}

fn ceil_f32(_store: &mut dyn VMStore, _instance: InstanceId, val: f32) -> f32 {
    val.wasm_ceil()
}

fn ceil_f64(_store: &mut dyn VMStore, _instance: InstanceId, val: f64) -> f64 {
    val.wasm_ceil()
}

fn trunc_f32(_store: &mut dyn VMStore, _instance: InstanceId, val: f32) -> f32 {
    val.wasm_trunc()
}

fn trunc_f64(_store: &mut dyn VMStore, _instance: InstanceId, val: f64) -> f64 {
    val.wasm_trunc()
}

fn nearest_f32(_store: &mut dyn VMStore, _instance: InstanceId, val: f32) -> f32 {
    val.wasm_nearest()
}

fn nearest_f64(_store: &mut dyn VMStore, _instance: InstanceId, val: f64) -> f64 {
    val.wasm_nearest()
}

// This intrinsic is only used on x86_64 platforms as an implementation of
// the `i8x16.swizzle` instruction when `pshufb` in SSSE3 is not available.
#[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
fn i8x16_swizzle(_store: &mut dyn VMStore, _instance: InstanceId, a: i8x16, b: i8x16) -> i8x16 {
    union U {
        reg: i8x16,
        mem: [u8; 16],
    }

    unsafe {
        let a = U { reg: a }.mem;
        let b = U { reg: b }.mem;

        // Use the `swizzle` semantics of returning 0 on any out-of-bounds
        // index, rather than the x86 pshufb semantics, since Wasmtime uses
        // this to implement `i8x16.swizzle`.
        let select = |arr: &[u8; 16], byte: u8| {
            if byte >= 16 { 0x00 } else { arr[byte as usize] }
        };

        U {
            mem: [
                select(&a, b[0]),
                select(&a, b[1]),
                select(&a, b[2]),
                select(&a, b[3]),
                select(&a, b[4]),
                select(&a, b[5]),
                select(&a, b[6]),
                select(&a, b[7]),
                select(&a, b[8]),
                select(&a, b[9]),
                select(&a, b[10]),
                select(&a, b[11]),
                select(&a, b[12]),
                select(&a, b[13]),
                select(&a, b[14]),
                select(&a, b[15]),
            ],
        }
        .reg
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "sse")))]
fn i8x16_swizzle(_store: &mut dyn VMStore, _instance: InstanceId, _a: i8x16, _b: i8x16) -> i8x16 {
    unreachable!()
}

// This intrinsic is only used on x86_64 platforms as an implementation of
// the `i8x16.shuffle` instruction when `pshufb` in SSSE3 is not available.
#[cfg(all(target_arch = "x86_64", target_feature = "sse"))]
fn i8x16_shuffle(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    a: i8x16,
    b: i8x16,
    c: i8x16,
) -> i8x16 {
    union U {
        reg: i8x16,
        mem: [u8; 16],
    }

    unsafe {
        let ab = [U { reg: a }.mem, U { reg: b }.mem];
        let c = U { reg: c }.mem;

        // Use the `shuffle` semantics of returning 0 on any out-of-bounds
        // index, rather than the x86 pshufb semantics, since Wasmtime uses
        // this to implement `i8x16.shuffle`.
        let select = |arr: &[[u8; 16]; 2], byte: u8| {
            if byte >= 32 {
                0x00
            } else if byte >= 16 {
                arr[1][byte as usize - 16]
            } else {
                arr[0][byte as usize]
            }
        };

        U {
            mem: [
                select(&ab, c[0]),
                select(&ab, c[1]),
                select(&ab, c[2]),
                select(&ab, c[3]),
                select(&ab, c[4]),
                select(&ab, c[5]),
                select(&ab, c[6]),
                select(&ab, c[7]),
                select(&ab, c[8]),
                select(&ab, c[9]),
                select(&ab, c[10]),
                select(&ab, c[11]),
                select(&ab, c[12]),
                select(&ab, c[13]),
                select(&ab, c[14]),
                select(&ab, c[15]),
            ],
        }
        .reg
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "sse")))]
fn i8x16_shuffle(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    _a: i8x16,
    _b: i8x16,
    _c: i8x16,
) -> i8x16 {
    unreachable!()
}

fn fma_f32x4(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    x: f32x4,
    y: f32x4,
    z: f32x4,
) -> f32x4 {
    union U {
        reg: f32x4,
        mem: [f32; 4],
    }

    unsafe {
        let x = U { reg: x }.mem;
        let y = U { reg: y }.mem;
        let z = U { reg: z }.mem;

        U {
            mem: [
                x[0].wasm_mul_add(y[0], z[0]),
                x[1].wasm_mul_add(y[1], z[1]),
                x[2].wasm_mul_add(y[2], z[2]),
                x[3].wasm_mul_add(y[3], z[3]),
            ],
        }
        .reg
    }
}

fn fma_f64x2(
    _store: &mut dyn VMStore,
    _instance: InstanceId,
    x: f64x2,
    y: f64x2,
    z: f64x2,
) -> f64x2 {
    union U {
        reg: f64x2,
        mem: [f64; 2],
    }

    unsafe {
        let x = U { reg: x }.mem;
        let y = U { reg: y }.mem;
        let z = U { reg: z }.mem;

        U {
            mem: [x[0].wasm_mul_add(y[0], z[0]), x[1].wasm_mul_add(y[1], z[1])],
        }
        .reg
    }
}

/// This intrinsic is just used to record trap information.
///
/// The `Infallible` "ok" type here means that this never returns success, it
/// only ever returns an error, and this hooks into the machinery to handle
/// `Result` values to record such trap information.
fn trap(_store: &mut dyn VMStore, _instance: InstanceId, code: u8) -> Result<Infallible> {
    match CompiledTrap::from_u8(code).unwrap() {
        CompiledTrap::Normal(trap) => Err(trap.into()),
        CompiledTrap::InternalAssert => bail_bug!("internal assert hit in wasm"),
        CompiledTrap::GcHeapCorrupt => bail_bug!("GC heap corruption detected"),
    }
}

fn raise(store: &mut dyn VMStore, _instance: InstanceId) {
    // SAFETY: this is only called from compiled wasm so we know that wasm has
    // already been entered. It's a dynamic safety precondition that the trap
    // information has already been arranged to be present.
    unsafe { crate::runtime::vm::traphandlers::raise_preexisting_trap(store) }
}

// Builtins for continuations. These are thin wrappers around the
// respective definitions in stack_switching.rs.
#[cfg(feature = "stack-switching")]
fn cont_new(
    store: &mut dyn VMStore,
    instance: InstanceId,
    func: *mut u8,
    param_count: u32,
    result_count: u32,
) -> Result<Option<AllocationSize>> {
    let ans =
        crate::vm::stack_switching::cont_new(store, instance, func, param_count, result_count)?;
    Ok(Some(AllocationSize(ans.cast::<u8>() as usize)))
}

#[cfg(feature = "gc")]
fn get_instance_id(_store: &mut dyn VMStore, instance: InstanceId) -> u32 {
    instance.as_u32()
}

#[cfg(feature = "gc")]
fn throw_ref(store: &mut dyn VMStore, _instance: InstanceId, exnref: u32) -> Result<()> {
    let exnref = VMGcRef::from_raw_u32(exnref).ok_or_else(|| Trap::NullReference)?;
    Err(store.set_pending_exception(&exnref))
}

fn breakpoint(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
    #[cfg(feature = "debug")]
    {
        store.block_on_debug_handler(crate::DebugEvent::Breakpoint)?;
    }
    // Avoid unused-argument warning in no-debugger builds.
    let _ = store;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsContextMut;
    use crate::runtime::transaction::{
        OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT, ObjectId, ObjectKind, ObjectPayload,
        encode_object_record_for_recovery, type_layout::TypeLayoutRegistry,
    };

    #[cfg(all(feature = "gc", feature = "transaction"))]
    #[test]
    fn transaction_externalization_does_not_leak_lifo_roots() {
        let engine = crate::Engine::default();
        let mut store = crate::Store::new(&engine, ());
        let before = store.as_context_mut().0.gc_roots().enter_lifo_scope();

        for raw in [1, 3, 5, 7] {
            let context = store.as_context_mut();
            transaction_textern_convert_tany(context.0, InstanceId::from_u32(0), raw).unwrap();
        }

        let after = store.as_context_mut().0.gc_roots().enter_lifo_scope();
        assert_eq!(after, before, "externalization leaked hidden LIFO roots");
    }

    #[cfg(all(feature = "gc", feature = "transaction"))]
    #[test]
    fn transaction_external_handle_recovery_does_not_leak_lifo_roots() {
        let engine = crate::Engine::default();
        let mut store = crate::Store::new(&engine, ());
        let _transaction_scope = store.as_context_mut().0.transaction_enter_extern_scope();
        let handle = {
            let mut scope = crate::RootScope::new(&mut store);
            let reference = ExternRef::new(&mut scope, 761_u32).unwrap();
            scope
                .as_context_mut()
                .0
                .transaction_extern_handle(reference)
                .unwrap()
        };

        store.gc(None).unwrap();
        let before = store.as_context_mut().0.gc_roots().enter_lifo_scope();
        for _ in 0..4 {
            let context = store.as_context_mut();
            let raw = transaction_textern_convert_tany(context.0, InstanceId::from_u32(0), handle)
                .unwrap();
            assert_ne!(raw, 0);
            store.gc(None).unwrap();
        }
        let after = store.as_context_mut().0.gc_roots().enter_lifo_scope();

        assert_eq!(after, before, "handle recovery leaked hidden LIFO roots");
    }

    #[cfg(all(feature = "gc", feature = "transaction"))]
    #[test]
    fn malformed_externalized_result_aborts_active_transaction() {
        let engine = crate::Engine::default();
        let mut store = crate::Store::new(&engine, ());
        store.transaction_state_mut().begin().unwrap();

        let context = store.as_context_mut();
        let error =
            transaction_preserve_textern_result(context.0, InstanceId::from_u32(0), u32::MAX)
                .unwrap_err();

        assert!(error.to_string().contains("GC heap"), "{error:?}");
        assert_eq!(store.transaction_state().active_transaction(), None);
    }

    #[cfg(feature = "transaction-mvcc")]
    #[test]
    fn transaction_constructor_boundary_existing_operation_and_abort_errors_are_combined() {
        let engine = crate::Engine::default();
        let mut store = crate::Store::new(&engine, ());
        let context = store.as_context_mut();
        let vm_store = context.0;
        let runtime = vm_store.store_opaque().transaction_region_runtime().clone();
        let visibility = runtime.visibility_for_test();

        assert!(begin_transaction_constructor_boundary(vm_store).unwrap());
        runtime.fail_release_transaction_once_for_test();
        let began = begin_transaction_constructor_boundary(vm_store).unwrap();
        assert!(!began);
        let operation = Err::<u32, _>(crate::format_err!("injected constructor operation failure"));
        let finish = finish_transaction_constructor_boundary(vm_store, began, &operation);
        let error = combine_operation_and_cleanup_results(
            operation,
            finish,
            "failed to finish transaction constructor boundary",
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("injected constructor operation failure"),
            "{message}"
        );
        assert!(
            message.contains("injected release transaction failure"),
            "{message}"
        );
        assert_eq!(
            vm_store
                .store_opaque()
                .transaction_state()
                .active_transaction(),
            None
        );
        assert_eq!(
            visibility.snapshot_lifecycle_counts_for_test().unwrap(),
            (0, 1, 1, 0)
        );
    }

    #[cfg(feature = "transaction-mvcc")]
    #[test]
    fn transaction_constructor_boundary_commit_start_failure_aborts_auto_started_transaction() {
        let engine = crate::Engine::default();
        let mut store = crate::Store::new(&engine, ());
        let context = store.as_context_mut();
        let vm_store = context.0;
        let runtime = vm_store.store_opaque().transaction_region_runtime().clone();
        let visibility = runtime.visibility_for_test();

        let began = begin_transaction_constructor_boundary(vm_store).unwrap();
        assert!(began);
        runtime.poison_lock_authority_for_test();
        let operation = Ok(7u32);
        let finish = finish_transaction_constructor_boundary(vm_store, began, &operation);
        let error = combine_operation_and_cleanup_results(
            operation,
            finish,
            "failed to finish transaction constructor boundary",
        )
        .unwrap_err();
        assert!(error.to_string().contains("lock poisoned"), "{error:?}");
        assert_eq!(
            vm_store
                .store_opaque()
                .transaction_state()
                .active_transaction(),
            None
        );
        assert_eq!(
            visibility.snapshot_lifecycle_counts_for_test().unwrap(),
            (0, 1, 1, 0)
        );
    }

    fn recovered_struct_winner(
        source: &crate::runtime::vm::block_region::SyntheticRecoveredWinnerSourceHandle,
        object_id: ObjectId,
        version: u32,
        fields: Vec<ObjectValue>,
    ) -> crate::runtime::vm::RecoveredObjectWinner {
        let record_bytes = encode_object_record_for_recovery(
            object_id.object_index,
            version,
            ObjectKind::Struct as u16,
            crate::runtime::transaction::type_layout::TypeLayoutId::DEFAULT_STRUCT.get(),
            &ObjectPayload::Struct(fields),
        )
        .unwrap();
        let data_record = crate::runtime::vm::TMemory::encode_publication_data_record(
            crate::runtime::vm::pack_object_granule_id(
                crate::runtime::vm::PackedGranuleDomain::TStruct,
                object_id.object_index,
            )
            .unwrap(),
            version,
            crate::runtime::vm::PackedGranuleDomain::TStruct as u16,
            crate::runtime::transaction::type_layout::TypeLayoutId::DEFAULT_STRUCT.get(),
            &record_bytes,
        )
        .unwrap();
        let data_record_offset =
            crate::runtime::vm::block_region::register_synthetic_recovered_winner_data_record_for_test(
                source,
                &data_record,
            );
        crate::runtime::vm::RecoveredObjectWinner {
            object_id: object_id.object_index,
            version,
            kind: ObjectKind::Struct as u16,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::DEFAULT_STRUCT
                .get(),
            data_block: 0,
            data_offset: 0,
            data_record_offset,
            record_len: record_bytes.len() as u64,
        }
    }

    #[test]
    fn object_value_abi_roundtrips_transaction_handle_for_large_object_id() {
        let missing_full_width_object_id = ObjectId {
            object_index: u64::from(u32::MAX) + 1,
        };
        let durable_raw =
            crate::runtime::transaction::PersistentObjectRefRaw::from_optional_object_id(Some(
                missing_full_width_object_id,
            ))
            .unwrap();
        assert!(durable_raw.as_raw() > u64::from(u32::MAX));

        // Sparse full-width object ids are durable-only today because live slots are Vec-backed.
        let root = ObjectId { object_index: 41 };
        let child = ObjectId { object_index: 42 };
        let mut objects = ObjectTable::default();
        let recovered_fixture_source =
            crate::runtime::vm::block_region::new_synthetic_recovered_winner_source_for_test();
        let recovered_source =
            crate::runtime::vm::block_region::synthetic_recovered_winner_source_for_test(
                &recovered_fixture_source,
            );
        objects
            .rebuild_reachable_from_recovery_with_source_for_test(
                &TypeLayoutRegistry::default(),
                &[
                    recovered_struct_winner(
                        &recovered_fixture_source,
                        child,
                        1,
                        vec![ObjectValue::I32(9)],
                    ),
                    recovered_struct_winner(
                        &recovered_fixture_source,
                        root,
                        1,
                        vec![ObjectValue::Ref(Some(child))],
                    ),
                ],
                &[root.object_index],
                recovered_source.clone(),
            )
            .unwrap();
        let expected_handle = objects.transaction_ref_handle_for_object_id(child).unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let mut state = TransactionState::default();

        let abi = live_transaction_abi_from_object_value(
            &mut durable_refs,
            &mut state,
            &mut objects,
            &ObjectValue::Ref(Some(child)),
        )
        .unwrap();
        let (tag, low, high) = abi.as_parts();
        assert_eq!(tag, OBJECT_VALUE_ABI_TAG_REF);
        assert_eq!(low, u64::from(expected_handle));
        assert_eq!(high, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT);
        assert_eq!(
            objects
                .object_id_for_transaction_ref_handle(u32::try_from(low).unwrap())
                .unwrap(),
            child
        );
        assert_eq!(
            object_value_from_transaction_abi(&mut durable_refs, Some(&state), &mut objects, abi)
                .unwrap(),
            ObjectValue::Ref(Some(child))
        );
    }

    #[test]
    fn object_value_abi_roundtrips_recovered_persistent_object_ref_without_gc_ref() {
        let root = ObjectId { object_index: 41 };
        let child = ObjectId { object_index: 42 };
        let mut objects = ObjectTable::default();
        let recovered_fixture_source =
            crate::runtime::vm::block_region::new_synthetic_recovered_winner_source_for_test();
        let recovered_source =
            crate::runtime::vm::block_region::synthetic_recovered_winner_source_for_test(
                &recovered_fixture_source,
            );
        objects
            .rebuild_reachable_from_recovery_with_source_for_test(
                &TypeLayoutRegistry::default(),
                &[
                    recovered_struct_winner(
                        &recovered_fixture_source,
                        child,
                        1,
                        vec![ObjectValue::I32(9)],
                    ),
                    recovered_struct_winner(
                        &recovered_fixture_source,
                        root,
                        1,
                        vec![ObjectValue::Ref(Some(child))],
                    ),
                ],
                &[root.object_index],
                recovered_source.clone(),
            )
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let expected_handle = objects.transaction_ref_handle_for_object_id(child).unwrap();
        let mut state = TransactionState::default();

        let abi = live_transaction_abi_from_object_value(
            &mut durable_refs,
            &mut state,
            &mut objects,
            &ObjectValue::Ref(Some(child)),
        )
        .unwrap();
        let (tag, low, high) = abi.as_parts();
        assert_eq!(tag, OBJECT_VALUE_ABI_TAG_REF);
        assert_eq!(low, u64::from(expected_handle));
        assert_eq!(high, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT);
        assert_eq!(
            objects
                .object_id_for_transaction_ref_handle(u32::try_from(low).unwrap())
                .unwrap(),
            child
        );
        assert_eq!(
            object_value_from_transaction_abi(&mut durable_refs, Some(&state), &mut objects, abi)
                .unwrap(),
            ObjectValue::Ref(Some(child))
        );
    }

    #[test]
    fn transaction_table_range_bounds_use_staged_size_for_private_grow() {
        let engine = crate::Engine::default();
        let module = crate::Module::new(
            &engine,
            r#"
            (module
              (table $t 1 3 funcref))
            "#,
        )
        .unwrap();
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let instance_id = instance.id();
        let context = store.as_context_mut();
        let vm_store = context.0;

        {
            let state = vm_store.store_opaque_mut().transaction_state_mut();
            state.begin().unwrap();
            state
                .stage_table_size_owned(Some(instance_id), 0, 2)
                .unwrap();
        }

        transaction_ttable_read_range_impl(vm_store, instance_id, 0, 1, 1).unwrap();
        transaction_ttable_write_range_impl(vm_store, instance_id, 0, 1, 1).unwrap();
        assert!(transaction_ttable_write_range_impl(vm_store, instance_id, 0, 2, 1).is_err());

        vm_store
            .store_opaque_mut()
            .transaction_state_mut()
            .complete_commit()
            .unwrap();
    }

    #[test]
    fn explicit_persistent_object_live_kind_rejects_i31_overlap_without_object() {
        let raw = ObjectTable::encode_raw_i31_ref(21);
        let abi = ObjectValueAbi::from_live_parts(
            OBJECT_VALUE_ABI_TAG_REF,
            raw,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        )
        .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let mut objects = ObjectTable::default();

        let error = object_value_from_transaction_abi(&mut durable_refs, None, &mut objects, abi)
            .expect_err("explicit persistent object refs must not fall back to i31");
        assert!(
            error
                .to_string()
                .contains("unknown transaction object ref handle")
        );
    }

    #[test]
    fn explicit_persistent_object_live_kind_rejects_null_raw_ref() {
        let abi = ObjectValueAbi::from_live_parts(
            OBJECT_VALUE_ABI_TAG_REF,
            0,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        )
        .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let mut objects = ObjectTable::default();

        let error = object_value_from_transaction_abi(&mut durable_refs, None, &mut objects, abi)
            .expect_err("explicit persistent object refs must not decode null raw refs");
        assert!(
            error
                .to_string()
                .contains("transaction object ref handle cannot be zero")
        );
    }

    #[test]
    fn live_ref_bridge_resolves_transaction_handle_before_i31_fallback() {
        let object = ObjectId { object_index: 2 };
        let mut objects = ObjectTable::default();
        let recovered_fixture_source =
            crate::runtime::vm::block_region::new_synthetic_recovered_winner_source_for_test();
        let recovered_source =
            crate::runtime::vm::block_region::synthetic_recovered_winner_source_for_test(
                &recovered_fixture_source,
            );
        objects
            .rebuild_reachable_from_recovery_with_source_for_test(
                &TypeLayoutRegistry::default(),
                &[recovered_struct_winner(
                    &recovered_fixture_source,
                    object,
                    1,
                    vec![ObjectValue::I32(7)],
                )],
                &[object.object_index],
                recovered_source.clone(),
            )
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let handle = objects
            .transaction_ref_handle_for_object_id(object)
            .unwrap();
        let raw = u64::from(handle);
        assert!(!ObjectTable::is_raw_i31_ref(raw));

        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED,
                raw,
            )
            .unwrap(),
            ObjectValue::Ref(Some(object))
        );
        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
                raw,
            )
            .unwrap(),
            ObjectValue::Ref(Some(object))
        );
        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
                raw,
            )
            .unwrap(),
            ObjectValue::Ref(Some(object))
        );
    }

    #[test]
    fn live_ref_bridge_distinguishes_gc_bridge_refs_from_transaction_handles() {
        let gc_raw = 0x8000_0000;
        let mut objects = ObjectTable::default();
        let gc_object = objects
            .allocate_persistent_struct_for_gc_ref(gc_raw, vec![ObjectValue::I32(1)])
            .unwrap();
        let handle_object = objects
            .allocate_persistent_struct_for_gc_ref(0x716, vec![ObjectValue::I32(2)])
            .unwrap();
        let handle = objects
            .transaction_ref_handle_for_object_id(handle_object)
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();

        assert_ne!(handle, gc_raw);
        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
                u64::from(gc_raw),
            )
            .unwrap(),
            ObjectValue::Ref(Some(gc_object))
        );
        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
                u64::from(handle),
            )
            .unwrap(),
            ObjectValue::Ref(Some(handle_object))
        );
    }

    #[test]
    fn live_ref_bridge_avoids_registered_extern_refs_when_encoding_transaction_handles() {
        let reserved_raw = 0x8000_0000;
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 7,
            handle: 0x700,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let mut durable_refs = DurableReferenceRegistry::default();
        durable_refs
            .register_extern_ref(reserved_raw, extern_identity)
            .unwrap();
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x718, vec![ObjectValue::I32(2)])
            .unwrap();
        let mut state = TransactionState::default();

        let abi = live_transaction_abi_from_object_value(
            &durable_refs,
            &mut state,
            &mut objects,
            &ObjectValue::Ref(Some(object)),
        )
        .unwrap();
        let (tag, low, high) = abi.as_parts();
        assert_eq!(tag, OBJECT_VALUE_ABI_TAG_REF);
        assert_ne!(low, u64::from(reserved_raw));
        assert_eq!(high, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT);
        assert_eq!(
            objects
                .object_id_for_transaction_ref_handle(u32::try_from(low).unwrap())
                .unwrap(),
            object
        );
    }

    #[test]
    fn transaction_constructor_handle_allocation_avoids_registered_live_ref_raws() {
        let reserved_extern_raw = 0x8000_0000;
        let reserved_func_raw = 0x8000_0002usize;
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 9,
            handle: 0x900,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let func_identity = crate::runtime::transaction::DurableFuncIdentity {
            module_fingerprint: 0x991,
            function_index: 1,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let mut durable_refs = DurableReferenceRegistry::default();
        durable_refs
            .register_extern_ref(reserved_extern_raw, extern_identity)
            .unwrap();
        durable_refs
            .register_func_ref(reserved_func_raw, func_identity)
            .unwrap();
        let mut state = TransactionState::default();
        let mut objects = ObjectTable::default();

        state.begin().unwrap();

        let handle = allocate_transaction_array_record(
            &durable_refs,
            &mut state,
            &mut objects,
            vec![ObjectValue::I32(7)],
            None,
        )
        .unwrap()
        .get();
        assert_ne!(handle, reserved_extern_raw);
        assert_ne!(usize::try_from(handle).unwrap(), reserved_func_raw);
        assert_eq!(
            state.known_object_id_for_transaction_ref_handle(&objects, handle),
            Some(ObjectId {
                object_index: 1u64 << 63
            })
        );
        assert_eq!(objects.live_count(), 0);
    }

    #[test]
    fn live_ref_bridge_rejects_registered_extern_ref_colliding_with_transaction_handle() {
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 8,
            handle: 0x800,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let mut durable_refs = DurableReferenceRegistry::default();
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x719, vec![ObjectValue::I32(2)])
            .unwrap();
        let handle = objects
            .transaction_ref_handle_for_object_id(object)
            .unwrap();
        durable_refs
            .register_extern_ref(handle, extern_identity)
            .unwrap();

        let error = live_ref_value_from_raw(
            &mut durable_refs,
            None,
            &mut objects,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
            u64::from(handle),
        )
        .unwrap_err();
        assert!(error.to_string().contains(
            "live reference raw collides with multiple transaction reference namespaces"
        ));

        let error = live_ref_value_from_raw(
            &mut durable_refs,
            None,
            &mut objects,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
            u64::from(handle),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("live external reference raw collides with transaction object handle")
        );
    }

    #[test]
    fn durable_func_extern_live_abi_requires_rebind_after_recovery() {
        let func_identity = crate::runtime::transaction::DurableFuncIdentity {
            module_fingerprint: 0x4558_5446_554e_4301,
            function_index: 2,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_FUNC,
        };
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 0x4558,
            handle: 0x4558_5445_5854_0001,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let mut writer_refs = DurableReferenceRegistry::default();
        writer_refs
            .register_func_ref(0x5100, func_identity)
            .unwrap();
        writer_refs
            .register_extern_ref(0x5200, extern_identity)
            .unwrap();
        let mut objects = ObjectTable::default();

        let func_value = object_value_from_transaction_abi(
            &mut writer_refs,
            None,
            &mut objects,
            ObjectValueAbi::from_live_parts(
                OBJECT_VALUE_ABI_TAG_REF,
                0x5100,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC,
            )
            .unwrap(),
        )
        .unwrap();
        let extern_value = object_value_from_transaction_abi(
            &mut writer_refs,
            None,
            &mut objects,
            ObjectValueAbi::from_live_parts(
                OBJECT_VALUE_ABI_TAG_REF,
                0x5200,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(func_value, ObjectValue::FuncRef(func_identity));
        assert_eq!(extern_value, ObjectValue::ExternRef(extern_identity));

        let recovered_func = ObjectValueAbi::from_object_value(&func_value)
            .unwrap()
            .to_object_value()
            .unwrap();
        let recovered_extern = ObjectValueAbi::from_object_value(&extern_value)
            .unwrap()
            .to_object_value()
            .unwrap();
        let mut recovered_objects = ObjectTable::default();
        let empty_refs = DurableReferenceRegistry::default();
        let mut state = TransactionState::default();
        let error = live_transaction_abi_from_object_value(
            &empty_refs,
            &mut state,
            &mut recovered_objects,
            &recovered_func,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("durable function identity is not registered"),
            "{error:?}"
        );
        let error = live_transaction_abi_from_object_value(
            &empty_refs,
            &mut state,
            &mut recovered_objects,
            &recovered_extern,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("durable external identity is not registered"),
            "{error:?}"
        );

        let mut rebound_refs = DurableReferenceRegistry::default();
        rebound_refs
            .register_func_ref(0x6100, func_identity)
            .unwrap();
        rebound_refs
            .register_extern_ref(0x6200, extern_identity)
            .unwrap();
        assert_eq!(
            live_transaction_abi_from_object_value(
                &rebound_refs,
                &mut state,
                &mut recovered_objects,
                &recovered_func,
            )
            .unwrap()
            .as_parts(),
            (
                OBJECT_VALUE_ABI_TAG_REF,
                0x6100,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC
            )
        );
        assert_eq!(
            live_transaction_abi_from_object_value(
                &rebound_refs,
                &mut state,
                &mut recovered_objects,
                &recovered_extern,
            )
            .unwrap()
            .as_parts(),
            (
                OBJECT_VALUE_ABI_TAG_REF,
                0x6200,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
            )
        );
    }

    #[test]
    fn persistent_slot_gc_snapshot_preserves_registered_extern_ref() {
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 0x4558,
            handle: 0x4558_5445_5854_0002,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let mut durable_refs = DurableReferenceRegistry::default();
        durable_refs
            .register_extern_ref(0x5202, extern_identity)
            .unwrap();
        let mut objects = ObjectTable::default();
        let mut state = TransactionState::default();

        assert_eq!(
            gc_snapshot_raw_from_object_value(
                &durable_refs,
                &mut state,
                &mut objects,
                ObjectValue::ExternRef(extern_identity),
                "test GC reference"
            )
            .unwrap(),
            0x5202
        );
    }

    #[test]
    fn persistent_slot_gc_snapshot_rejects_unregistered_extern_ref() {
        let extern_identity = crate::runtime::transaction::DurableExternIdentity {
            namespace: 0x4558,
            handle: 0x4558_5445_5854_0003,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        let durable_refs = DurableReferenceRegistry::default();
        let mut objects = ObjectTable::default();
        let mut state = TransactionState::default();

        let error = gc_snapshot_raw_from_object_value(
            &durable_refs,
            &mut state,
            &mut objects,
            ObjectValue::ExternRef(extern_identity),
            "test GC reference",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("durable external identity is not registered"),
            "{error:?}"
        );
    }

    #[test]
    fn live_ref_bridge_rejects_untyped_gc_raw_that_only_matches_persistent_object_id() {
        let object = ObjectId { object_index: 41 };
        let gc_raw = object.object_index + 1;
        assert!(!ObjectTable::is_raw_i31_ref(gc_raw));

        let mut objects = ObjectTable::default();
        let recovered_fixture_source =
            crate::runtime::vm::block_region::new_synthetic_recovered_winner_source_for_test();
        let recovered_source =
            crate::runtime::vm::block_region::synthetic_recovered_winner_source_for_test(
                &recovered_fixture_source,
            );
        objects
            .rebuild_reachable_from_recovery_with_source_for_test(
                &TypeLayoutRegistry::default(),
                &[recovered_struct_winner(
                    &recovered_fixture_source,
                    object,
                    1,
                    vec![ObjectValue::I32(7)],
                )],
                &[object.object_index],
                recovered_source.clone(),
            )
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let handle = objects
            .transaction_ref_handle_for_object_id(object)
            .unwrap();

        let err = live_ref_value_from_raw(
            &mut durable_refs,
            None,
            &mut objects,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
            gc_raw,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown transactional object GC ref"),
            "{err:?}"
        );

        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
                u64::from(handle),
            )
            .unwrap(),
            ObjectValue::Ref(Some(object))
        );
    }

    #[test]
    fn live_ref_bridge_keeps_i31_when_overlap_is_not_live_persistent_object() {
        let i31 = 21;
        let raw = ObjectTable::encode_raw_i31_ref(i31);
        assert_eq!(raw, 43);

        let mut objects = ObjectTable::default();
        let mut durable_refs = DurableReferenceRegistry::default();

        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED,
                raw,
            )
            .unwrap(),
            ObjectValue::I31(i31)
        );
        assert_eq!(
            live_ref_value_from_raw(
                &mut durable_refs,
                None,
                &mut objects,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
                raw,
            )
            .unwrap(),
            ObjectValue::I31(i31)
        );
    }

    #[test]
    fn collect_tmemory_participants_groups_granules_by_memory() {
        let owner = InstanceId::from_u32(3);
        let records = vec![
            StagedRecord::MemoryGranule {
                owner_instance: Some(owner),
                memory_index: 7,
                granule_index: 0,
                bytes: vec![1, 2],
            },
            StagedRecord::MemoryGranule {
                owner_instance: Some(owner),
                memory_index: 7,
                granule_index: 1,
                bytes: vec![3, 4],
            },
        ];

        let participants = collect_tmemory_participants(InstanceId::from_u32(0), &records);

        let participant = TMemoryParticipant {
            owner,
            owner_instance_key: Some(owner),
            memory_index: 7,
        };
        assert_eq!(participants.len(), 1);
        assert_eq!(
            participants.get(&participant).unwrap(),
            &vec![(0, vec![1, 2]), (1, vec![3, 4])]
        );
    }

    #[test]
    fn collect_tmemory_participants_allows_distinct_persistent_memories() {
        let owner = InstanceId::from_u32(3);
        let records = vec![
            StagedRecord::MemoryGranule {
                owner_instance: Some(owner),
                memory_index: 7,
                granule_index: 0,
                bytes: vec![1],
            },
            StagedRecord::MemoryGranule {
                owner_instance: Some(owner),
                memory_index: 8,
                granule_index: 0,
                bytes: vec![2],
            },
        ];

        let participants = collect_tmemory_participants(InstanceId::from_u32(0), &records);

        assert_eq!(participants.len(), 2);
        assert!(participants.contains_key(&TMemoryParticipant {
            owner,
            owner_instance_key: Some(owner),
            memory_index: 7,
        }));
        assert!(participants.contains_key(&TMemoryParticipant {
            owner,
            owner_instance_key: Some(owner),
            memory_index: 8,
        }));
    }

    #[test]
    fn collect_tmemory_participants_preserves_implicit_owner_key() {
        let default_owner = InstanceId::from_u32(9);
        let records = vec![StagedRecord::MemoryGranule {
            owner_instance: None,
            memory_index: 2,
            granule_index: 4,
            bytes: vec![5],
        }];

        let participants = collect_tmemory_participants(default_owner, &records);

        let participant = TMemoryParticipant {
            owner: default_owner,
            owner_instance_key: None,
            memory_index: 2,
        };
        assert_eq!(participants.get(&participant).unwrap(), &vec![(4, vec![5])]);
    }
}
