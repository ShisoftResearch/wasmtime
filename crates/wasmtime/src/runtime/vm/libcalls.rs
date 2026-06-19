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

use crate::bail_bug;
use crate::prelude::*;
use crate::runtime::store::{Asyncness, AutoAssertNoGc, InstanceId, StoreOpaque};
#[cfg(feature = "gc")]
use crate::runtime::transaction::DurableExternRefHostData;
use crate::runtime::transaction::{
    DurableReferenceRegistry, GlobalSnapshot, GranuleId, OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC, OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_I31, OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED, OBJECT_VALUE_ABI_TAG_REF, ObjectKind, ObjectPayload,
    ObjectTable, ObjectValue, ObjectValueAbi, OrdinaryGcPromotionAdapter,
    OrdinaryGcPromotionSource, OrdinaryGcPromotionValue, PERSISTENT_OBJECT_ABI_SLOT_SIZE,
    PendingCommitLogEntry, StagedRecord, TMemoryAccessSnapshot, TMemoryBackend,
    TableElementSnapshot, TransactionId, TransactionState, WasmtimePersistentFieldLayout,
    WasmtimePersistentFieldLayoutAbi, collect_tmemory_access_snapshot,
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
use core::convert::Infallible;
use core::ptr::NonNull;
#[cfg(feature = "threads")]
use core::time::Duration;
use wasmtime_core::math::WasmFloat;
#[cfg(feature = "gc")]
use wasmtime_environ::GcLayout;
use wasmtime_environ::{
    CompiledTrap, DefinedMemoryIndex, DefinedTableIndex, FuncIndex, GlobalIndex, MemoryIndex,
    PassiveElemIndex, TableIndex, Trap, TypeIndex, VMGcKind, VMSharedTypeIndex, WasmHeapTopType,
    WasmValType,
};
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
// store-local transaction state and per-instance `tmemory` sidecars. Remaining
// `SHISOFT-TWASM-MOCK` tags mark explicit research boundaries such as live-only
// reference bridge fallbacks; committed object/root and linear-memory paths use
// the durable transaction machinery.
fn transaction_enter_tfunc(store: &mut dyn VMStore, _instance: InstanceId) -> Result<u32> {
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

fn transaction_begin(store: &mut dyn VMStore, _instance: InstanceId) -> Result<()> {
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
    abort_active_transaction_on_error(store, &result);
    result
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
    abort_active_transaction_on_error(store, &result);
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

    let (records, read_granules) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        if state.structured_failure_pending() {
            state.clear_structured_failure();
            return Ok(());
        }
        if state.active_transaction().is_none() {
            return Ok(());
        }
        (state.staged_records()?, state.active_read_granules()?)
    };

    for granule in read_granules {
        let current_version = current_granule_version(store, instance, granule)?;
        store
            .store_opaque_mut()
            .transaction_state_mut()
            .validate_active_read(granule, current_version)?;
    }

    {
        let store = store.store_opaque_mut();
        let (engine, gc_store, durable_refs, state, object_table) =
            store.transaction_promotion_context_mut();
        let mut adapter =
            StoreBackedOrdinaryGcPromotionAdapter::new(engine, gc_store, durable_refs);
        state
            .promote_persistent_references_before_commit_with_adapter(object_table, &mut adapter)?;
        state.validate_active_object_reads(&*object_table)?;
    }

    let (stream_id, txid) = {
        let state = store.store_opaque_mut().transaction_state_mut();
        let transaction_id = state.active_transaction_required_raw()?;
        let stream_id = if let Some(region) = state.shared_region_runtime_for_publication() {
            region.current_thread_log_segment()?.stream_id()
        } else {
            u32::try_from(transaction_id)
                .context("transaction id does not fit durable transaction stream id")?
        };
        let txid = u32::try_from(transaction_id)
            .context("transaction id does not fit durable transaction id")?;
        (stream_id, txid)
    };

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

    let (object_publications, root_delta, persistent_gc_delta) = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let mut object_publications = Vec::new();
        state.commit_object_payloads_into(object_table, &mut object_publications)?;
        let root_delta = state.staged_persistent_root_delta(&*object_table)?;
        let root_publications = state.persistent_root_publications(&root_delta)?;
        let persistent_gc_delta =
            state.persistent_gc_commit_delta(&*object_table, &object_publications)?;
        object_publications.extend(root_publications);
        (object_publications, root_delta, persistent_gc_delta)
    };
    if !object_publications.is_empty() {
        let object_marker = {
            let store = store.store_opaque_mut();
            let (state, object_table) = store.transaction_state_and_object_table_mut();
            state.publish_object_publications_before_commit(
                stream_id,
                txid,
                &*object_table,
                &object_publications,
            )?
        };
        final_marker = object_marker.or(final_marker);
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
            let cleanup_result = store
                .store_opaque_mut()
                .transaction_state_mut()
                .finish_committed_cleanup_after_durable_commit_error();
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
    // The transaction is already committed at this point. Persistent GC
    // observation is opportunistic runtime maintenance and must not turn a
    // completed commit into an apparent failure.
    let _ = state.observe_persistent_gc_commit_delta_after_commit_best_effort(
        &*object_table,
        &persistent_gc_delta,
    );
    Ok(())
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

fn transaction_helper_i31_for_ref(
    store: &mut dyn VMStore,
    _instance: InstanceId,
    gc_ref: u32,
) -> u32 {
    let object_table = store.store_opaque_mut().transaction_object_table_mut();
    let Ok(object_id) = object_table.object_id_for_live_bridge_transaction_ref_raw(gc_ref) else {
        return 0;
    };
    let Ok(ObjectPayload::Struct(fields)) = object_table.payload(object_id) else {
        return 0;
    };
    let Some(value) = fields.get(1) else {
        return 0;
    };
    let value = match value {
        ObjectValue::I31(value) => *value,
        ObjectValue::I32(value) => *value,
        ObjectValue::I64(value) => *value as i32,
        ObjectValue::F32(value) => *value as i32,
        ObjectValue::F64(value) => *value as i32,
        // Generated proposal fixtures sometimes route a vector payload through
        // a `ti31` helper extraction. There is no scalar-preserving conversion
        // for that shape, so keep the compatibility path deterministic.
        ObjectValue::V128(_) => 0,
        ObjectValue::FuncRef(_) | ObjectValue::ExternRef(_) => 0,
        ObjectValue::Ref(_) => return 0,
    };
    (value as u32).wrapping_shl(1) | 1
}

const TRANSACTION_TREF_TEST_NOT_TRANSACTION: u32 = u32::MAX;
const TRANSACTION_TREF_TEST_KIND_EQ: u32 = 1;
const TRANSACTION_TREF_TEST_KIND_STRUCT: u32 = 3;
const TRANSACTION_TREF_TEST_KIND_ARRAY: u32 = 4;
const TRANSACTION_TREF_TEST_EXPECTED_TYPE_NONE: u32 = u32::MAX;

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

    let object_table = store.store_opaque().transaction_object_table();
    let Some(object_id) = object_table.known_object_id_for_transaction_ref_handle(raw_ref) else {
        return TRANSACTION_TREF_TEST_NOT_TRANSACTION;
    };
    let Ok(kind) = object_table.kind(object_id) else {
        return TRANSACTION_TREF_TEST_NOT_TRANSACTION;
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
        _ => return TRANSACTION_TREF_TEST_NOT_TRANSACTION,
    };
    if !abstract_match {
        return 0;
    }

    if expected_engine_type == TRANSACTION_TREF_TEST_EXPECTED_TYPE_NONE {
        return 1;
    }

    let Ok(Some(actual)) = object_table.runtime_type_index(object_id) else {
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
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tref_cast_read_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
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
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tref_cast_write_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    ref_handle: u32,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
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
    abort_active_transaction_on_error(store, &result);
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
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_set_v128(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_tglobal_set_v128_impl(store, instance, global, value);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tglobal_get_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;

    let (global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let staged = {
        let state = store.store_opaque_mut().transaction_state_mut();
        ensure!(
            state.active_transaction().is_some(),
            "transaction operation requires an active transaction"
        );
        state.acquire_global_read_owned(Some(instance), global_index.as_u32())?;
        state.staged_global_owned(Some(instance), global_index.as_u32())
    };
    let snapshot = match staged {
        Some(snapshot) => snapshot,
        None => read_global_snapshot(store, instance, global_index, wasm_ty)?,
    };
    ensure_global_snapshot_type(snapshot, wasm_ty)?;

    let snapshot = live_transaction_global_snapshot_for_read(store, snapshot)?;
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

    let (global_index, wasm_ty) = transaction_global(store, instance, global)?;
    let snapshot = global_snapshot_from_tag(tag, value)?;
    ensure_global_snapshot_type(snapshot, wasm_ty)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_global_owned(Some(instance), global_index.as_u32(), snapshot)?;
    Ok(())
}

fn transaction_tglobal_set_v128_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
    value: *mut u8,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;

    let (global_index, wasm_ty) = transaction_global(store, instance, global)?;
    ensure!(
        matches!(wasm_ty, WasmValType::V128),
        "transactional global value tag does not match global type"
    );
    let snapshot = GlobalSnapshot::V128(unsafe { *value.cast::<[u8; 16]>() });
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_global_owned(Some(instance), global_index.as_u32(), snapshot)?;
    Ok(())
}

fn transaction_global(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: u32,
) -> Result<(GlobalIndex, WasmValType)> {
    let global = GlobalIndex::from_u32(global);
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let module = instance_ref.env_module();
    let wasm_ty = module.globals[global].wasm_ty;
    Ok((global, wasm_ty))
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
) -> Result<GlobalSnapshot> {
    let GlobalSnapshot::GcRef(gc_ref) = snapshot else {
        return Ok(snapshot);
    };
    if gc_ref == 0 || ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
        return Ok(snapshot);
    }

    let store = store.store_opaque_mut();
    let (engine, gc_store, durable_refs, state, object_table) =
        store.transaction_promotion_context_mut();
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
        return Ok(snapshot);
    };
    let handle = object_table.transaction_ref_handle_for_object_id_avoiding(object_id, |raw| {
        live_ref_raw_is_registered(&*durable_refs, raw)
    })?;
    Ok(GlobalSnapshot::GcRef(handle))
}

fn read_global_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: GlobalIndex,
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
                Ok(GlobalSnapshot::GcRef(unsafe {
                    global.as_gc_ref().map_or(0, VMGcRef::as_raw_u32)
                }))
            }
            WasmHeapTopType::Cont => bail!("transactional contref global is not implemented yet"),
        },
    }
}

fn global_definition_ptr(
    store: &mut dyn VMStore,
    instance: InstanceId,
    global: GlobalIndex,
) -> Result<NonNull<vm::VMGlobalDefinition>> {
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let module = instance_ref.env_module();
    if let Some(defined) = module.defined_global_index(global) {
        return Ok(instance_ref.global_ptr(defined));
    }
    Ok(instance_ref.imported_global(global).from.as_non_null())
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
            GlobalSnapshot::FuncRef(value) => {
                *global.as_func_ref_mut() = core::ptr::with_exposed_provenance_mut(value);
            }
        }
    }
    Ok(())
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
    abort_active_transaction_on_error(store, &result);
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

    let state = store.store_opaque_mut().transaction_state_mut();
    let bytes = state.read_tmemory_owned_from_snapshot(
        Some(instance),
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
    abort_active_transaction_on_error(store, &result);
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

    let state = store.store_opaque_mut().transaction_state_mut();
    let bytes = state.read_tmemory_owned_from_snapshot(
        Some(instance),
        memory_index.as_u32(),
        effective,
        len,
        &snapshot,
    )?;
    state.set_tmemory_store_scratch(instance, memory_index.as_u32(), effective, bytes)
}

fn transaction_tmemory_size(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
) -> Result<*mut u8> {
    let result = transaction_tmemory_size_impl(store, instance, memory);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_size_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_memory_size_read_owned(Some(instance), memory_index.as_u32())?;
        if let Some(pages) = state.staged_memory_size_owned(Some(instance), memory_index.as_u32()) {
            return Ok(
                usize::try_from(pages).context("transactional memory size overflow")? as *mut u8,
            );
        }
    }
    let pages = {
        let instance_ref = store.instance_mut(instance);
        let instance_ref = instance_ref.as_ref();
        let tmemory = instance_ref
            .get_tmemory(memory_index)
            .context("transactional memory operation targeted non-transactional memory")?;
        tmemory.byte_len() / crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE
    };
    Ok(pages as *mut u8)
}

fn transaction_tmemory_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    delta: u64,
) -> Result<Option<AllocationSize>> {
    let result = transaction_tmemory_grow_impl(store, instance, memory, delta);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_grow_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    delta: u64,
) -> Result<Option<AllocationSize>> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let committed_pages = {
        let instance_ref = store.instance_mut(instance);
        let instance_ref = instance_ref.as_ref();
        let tmemory = instance_ref
            .get_tmemory(memory_index)
            .context("transactional memory operation targeted non-transactional memory")?;
        u64::try_from(tmemory.byte_len() / crate::runtime::vm::memory::tmemory::WASM_PAGE_SIZE)
            .context("tmemory previous size overflow")?
    };
    let previous_pages = store
        .store_opaque_mut()
        .transaction_state_mut()
        .staged_memory_size_owned(Some(instance), memory_index.as_u32())
        .unwrap_or(committed_pages);
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
        .stage_memory_size_owned(Some(instance), memory_index.as_u32(), new_pages)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    let len = usize::try_from(len).context("tmemory fill length overflow")?;
    let (memory_index, snapshot) =
        collect_defined_tmemory_snapshot(store, instance, memory, dst, len)?;
    let bytes = alloc::vec![val as u8; len];
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            Some(instance),
            memory_index.as_u32(),
            dst,
            &bytes,
            &snapshot,
        )
}

fn transaction_tmemory_copy(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_memory: u32,
    src_memory: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    let result =
        transaction_tmemory_copy_impl(store, instance, dst_memory, src_memory, dst, src, len);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tmemory_copy_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    dst_memory: u32,
    src_memory: u32,
    dst: u64,
    src: u64,
    len: u64,
) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
    let len = usize::try_from(len).context("tmemory copy length overflow")?;
    let (src_memory_index, src_snapshot) =
        collect_defined_tmemory_snapshot(store, instance, src_memory, src, len)?;
    let bytes = store
        .store_opaque_mut()
        .transaction_state_mut()
        .read_tmemory_owned_from_snapshot(
            Some(instance),
            src_memory_index.as_u32(),
            src,
            len,
            &src_snapshot,
        )?;
    let (dst_memory_index, dst_snapshot) =
        collect_defined_tmemory_snapshot(store, instance, dst_memory, dst, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            Some(instance),
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
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
    let data = unsafe { data.add(src) };
    let bytes = unsafe { core::slice::from_raw_parts(data.cast_const(), len) };
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_tmemory_write_owned_from_snapshot(
            Some(instance),
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

fn transaction_tdata_drop(store: &mut dyn VMStore, instance: InstanceId, _data: u32) -> Result<()> {
    let result = transaction_tdata_drop_impl(store, instance);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_tdata_drop_impl(store: &mut dyn VMStore, instance: InstanceId) -> Result<()> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)
}

fn transaction_ttable_get(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
) -> Result<*mut u8> {
    let result = transaction_ttable_get_impl(store, instance, table, index);
    abort_active_transaction_on_error(store, &result);
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

fn table_element_snapshot_to_raw(value: TableElementSnapshot) -> *mut u8 {
    match value {
        TableElementSnapshot::FuncRef(value) => core::ptr::with_exposed_provenance_mut(value),
        TableElementSnapshot::GcRef(value) => {
            core::ptr::with_exposed_provenance_mut(usize::try_from(value).unwrap())
        }
    }
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
    ensure_active_transaction(store)?;
    ensure_transaction_table_index_in_bounds(store, instance, table, index)?;
    {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_table_granule_read_owned(Some(instance), table, index, 0)?;
        if let Some(value) = state.staged_table_element_owned(Some(instance), table, index) {
            return Ok(table_element_snapshot_to_raw(value));
        }
    }
    let committed_size = u64::try_from(defined_table_size(store, instance, table)?)
        .context("defined table size does not fit u64")?;
    ensure!(
        index < committed_size,
        "transactional table overlay is missing staged element {index} in grown table"
    );

    let table_index = DefinedTableIndex::from_u32(table);
    let mut store = AutoAssertNoGc::new(store.store_opaque_mut());
    let (_gc_store, registry, instance_ref) =
        store.optional_gc_store_and_registry_and_instance_mut(instance);
    let table_ref = instance_ref.get_defined_table_with_lazy_init(
        registry,
        table_index,
        core::iter::once(index),
    );
    Ok(match table_ref.element_type() {
        TableElementType::Func => match table_ref.get_func(index)? {
            Some(ptr) => ptr.as_ptr().cast(),
            None => core::ptr::null_mut(),
        },
        TableElementType::GcRef => {
            let raw = table_ref.get_gc_ref(index)?.map_or(0, VMGcRef::as_raw_u32);
            core::ptr::with_exposed_provenance_mut(usize::try_from(raw).unwrap())
        }
        TableElementType::Cont => bail!("transactional contref table is not implemented yet"),
    })
}

fn transaction_ttable_set(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    index: u64,
    value: *mut u8,
) -> Result<()> {
    let result = transaction_ttable_set_impl(store, instance, table, index, value);
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    ensure_transaction_table_index_in_bounds(store, instance, table, index)?;

    let snapshot = {
        let table_index = DefinedTableIndex::from_u32(table);
        let mut store_no_gc = AutoAssertNoGc::new(store.store_opaque_mut());
        let (_gc_store, _registry, instance_ref) =
            store_no_gc.optional_gc_store_and_registry_and_instance_mut(instance);
        let table_ref = instance_ref.get_defined_table(table_index);
        table_element_snapshot_from_raw(table_ref.element_type(), value)?
    };

    store
        .store_opaque_mut()
        .transaction_state_mut()
        .stage_table_element_owned(Some(instance), table, index, snapshot)?;
    Ok(())
}

fn transaction_ttable_read_range(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    start: u64,
    len: u64,
) -> Result<()> {
    let result = transaction_ttable_read_range_impl(store, instance, table, start, len);
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    ensure_transaction_table_range_in_bounds(store, instance, table, start, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .acquire_table_granule_write_range_owned(Some(instance), table, start, len, 0)?;
    Ok(())
}

fn transaction_ttable_size(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<*mut u8> {
    let result = transaction_ttable_size_impl(store, instance, table);
    abort_active_transaction_on_error(store, &result);
    result
}

fn transaction_ttable_size_impl(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
) -> Result<*mut u8> {
    flush_pending_tmemory_store(store, instance)?;
    ensure_active_transaction(store)?;
    {
        let state = store.store_opaque_mut().transaction_state_mut();
        state.acquire_table_size_read_owned(Some(instance), table, 0)?;
        if let Some(size) = state.staged_table_size_owned(Some(instance), table) {
            return Ok(
                usize::try_from(size).context("transactional table size overflow")? as *mut u8,
            );
        }
    }
    let size = defined_table_size(store, instance, table)?;
    Ok(size as *mut u8)
}

fn transaction_ttable_grow(
    store: &mut dyn VMStore,
    instance: InstanceId,
    table: u32,
    delta: u64,
    init: *mut u8,
) -> Result<Option<AllocationSize>> {
    let result = transaction_ttable_grow_impl(store, instance, table, delta, init);
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;

    let table_index = DefinedTableIndex::from_u32(table);
    let (committed_size, maximum, element_type) = {
        let mut store = AutoAssertNoGc::new(store.store_opaque_mut());
        let (_gc_store, _registry, instance_ref) =
            store.optional_gc_store_and_registry_and_instance_mut(instance);
        let table_ref = instance_ref.get_defined_table(table_index);
        (
            u64::try_from(table_ref.size())?,
            table_ref.maximum().map(u64::try_from).transpose()?,
            table_ref.element_type(),
        )
    };
    let init = table_element_snapshot_from_raw(element_type, init)?;
    let current_size = store
        .store_opaque_mut()
        .transaction_state_mut()
        .staged_table_size_owned(Some(instance), table)
        .unwrap_or(committed_size);
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
    let handle = result?;
    finish?;
    Ok(handle)
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
    ensure_active_transaction(store)?;
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
            object_table,
            *abi,
        )?);
    }
    let object_id = object_table.allocate_struct(values)?;
    object_table.set_runtime_type_index(object_id, runtime_type_index)?;
    state.record_allocated_object(object_id)?;
    transaction_object_ref_handle_for_object_id(durable_refs, object_table, object_id)
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
    abort_active_transaction_on_error(store, &result);
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

    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let mut values = Vec::with_capacity(field_count);
    for abi in fields {
        values.push(object_value_from_transaction_abi(
            durable_refs,
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
    transaction_object_ref_handle_for_object_id(durable_refs, object_table, object_id)
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let field = usize::try_from(field).context("transactional struct field index overflow")?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
    let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
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
    let bytes = result?;
    finish?;
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
    ensure_active_transaction(store)?;
    let field = usize::try_from(field).context("transactional struct field index overflow")?;
    let abi = {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
        let value = state.read_struct_field(object_table, object_id, field)?;
        live_transaction_abi_from_object_value(durable_refs, object_table, &value)?
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
    let handle = result?;
    finish?;
    Ok(handle)
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
    ensure_active_transaction(store)?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
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
    abort_active_transaction_on_error(store, &result);
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
    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
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
    transaction_object_ref_handle_for_object_id(durable_refs, object_table, object_id)
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
    let handle = result?;
    finish?;
    Ok(handle)
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
    ensure_active_transaction(store)?;
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
    abort_active_transaction_on_error(store, &result);
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

    let store = store.store_opaque_mut();
    let (durable_refs, object_table) = store.transaction_durable_refs_and_object_table_mut();
    let mut values = Vec::with_capacity(element_count);
    for abi in elements {
        values.push(object_value_from_transaction_abi(
            durable_refs,
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
    transaction_object_ref_handle_for_object_id(durable_refs, object_table, object_id)
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
    let handle = result?;
    finish?;
    Ok(handle)
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
    ensure_active_transaction(store)?;
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
    let handle = result?;
    finish?;
    Ok(handle)
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
    ensure_active_transaction(store)?;
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
    let values = decode_transaction_array_elem_values(durable_refs, object_table, bytes)?;
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
    let object_id = object_table.allocate_array(values)?;
    if let Some(runtime_type_index) = runtime_type_index {
        object_table.set_runtime_type_index(object_id, runtime_type_index)?;
    }
    state.record_allocated_object(object_id)?;
    transaction_object_ref_handle_for_object_id(durable_refs, object_table, object_id)
}

fn transaction_object_ref_handle_for_object_id(
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    ensure!(gc_ref != 0, "null tarray reference");
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
    let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    let abi = ObjectValueAbi::from_live_parts(tag, low, high)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let store = store.store_opaque_mut();
    let (durable_refs, state, object_table) =
        store.transaction_durable_refs_state_and_object_table_mut();
    ensure!(gc_ref != 0, "null tarray reference");
    let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
    let value = object_value_from_transaction_abi(durable_refs, object_table, abi)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
    ensure!(dst_gc_ref != 0, "null tarray reference");
    ensure!(src_gc_ref != 0, "null tarray reference");
    let dst_index = usize::try_from(dst_index).context("transactional array index overflow")?;
    let src_index = usize::try_from(src_index).context("transactional array index overflow")?;
    let len = usize::try_from(len).context("transactional array length overflow")?;
    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    let dst_object_id = object_table.object_id_for_transaction_ref_handle(dst_gc_ref)?;
    let src_object_id = object_table.object_id_for_transaction_ref_handle(src_gc_ref)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
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
    let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
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
    abort_active_transaction_on_error(store, &result);
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
    ensure_active_transaction(store)?;
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
    let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
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
    let values = decode_transaction_array_elem_values(durable_refs, object_table, bytes)?;
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
    let bytes = result?;
    finish?;
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
    ensure_active_transaction(store)?;
    let index = usize::try_from(index).context("transactional array index overflow")?;
    let abi = {
        let store = store.store_opaque_mut();
        let (durable_refs, state, object_table) =
            store.transaction_durable_refs_state_and_object_table_mut();
        let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
        let value = state.read_array_element(object_table, object_id, index)?;
        live_transaction_abi_from_object_value(durable_refs, object_table, &value)?
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
    let bytes = result?;
    finish?;
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
    ensure_active_transaction(store)?;
    let len = {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        let object_id = object_table.object_id_for_transaction_ref_handle(gc_ref)?;
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
    object_table: &mut ObjectTable,
    abi: ObjectValueAbi,
) -> Result<ObjectValue> {
    let (tag, low, high) = abi.as_parts();
    if tag != OBJECT_VALUE_ABI_TAG_REF {
        return abi.to_object_value();
    }
    live_ref_value_from_raw(durable_refs, object_table, high, low)
}

fn live_transaction_abi_from_object_value(
    durable_refs: &DurableReferenceRegistry,
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
            let handle = object_table.transaction_ref_handle_for_object_id_avoiding(
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
    object_table: &ObjectTable,
    raw: u32,
    live_ref_kind: &str,
) -> Result<()> {
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
    object_table: &ObjectTable,
    raw: u32,
    include_func_refs: bool,
) -> Result<Option<ObjectValue>> {
    let func = include_func_refs
        .then(|| durable_refs.resolve_func_ref(raw as usize))
        .flatten();
    let extern_ = durable_refs.resolve_extern_ref(raw);
    let bridge_object = object_table.known_object_id_for_live_gc_ref_bridge(raw);
    let handle_object = object_table.known_object_id_for_transaction_ref_handle(raw);
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
    object_table: &mut ObjectTable,
    live_ref_kind: u64,
    raw: u64,
) -> Result<ObjectValue> {
    if live_ref_kind == OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT {
        let handle =
            u32::try_from(raw).context("live persistent object reference does not fit u32")?;
        let object_id = object_table.object_id_for_transaction_ref_handle(handle)?;
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
    let Some((owner, memory, addr, len)) = store
        .store_opaque_mut()
        .transaction_state_mut()
        .pending_memory_store()
    else {
        return Ok(());
    };
    let memory_index = MemoryIndex::from_u32(memory);
    let snapshot = collect_tmemory_snapshot(store, owner, memory_index, addr, len)?;
    store
        .store_opaque_mut()
        .transaction_state_mut()
        .flush_tmemory_store_scratch_from_snapshot(&snapshot)?;
    Ok(())
}

fn abort_active_transaction_on_error<T>(store: &mut dyn VMStore, result: &Result<T>) {
    if result.is_ok() {
        return;
    }

    let store = store.store_opaque_mut();
    let (state, object_table) = store.transaction_state_and_object_table_mut();
    if state.active_transaction().is_some() {
        let _ = state.abort_allocated_objects(object_table);
    }
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
        abort_active_transaction_on_error(store, result);
        return Ok(());
    }

    if result.is_ok() {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.begin_terminal_commit_with_object_cleanup(object_table)?;
        state.complete_commit()
    } else {
        let store = store.store_opaque_mut();
        let (state, object_table) = store.transaction_state_and_object_table_mut();
        state.abort_allocated_objects(object_table)
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
        let memory_index = MemoryIndex::from_u32(participant.memory_index);
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
                Some(participant.owner.as_u32()),
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
        let memory_index = MemoryIndex::from_u32(participant.memory_index);
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
            let memory_index = MemoryIndex::from_u32(*memory_index);
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
            let global_index = GlobalIndex::from_u32(*global_index);
            let mut global = global_definition_ptr(store, owner, global_index)?;
            let global = unsafe { global.as_mut() };
            write_global_snapshot(store.store_opaque_mut(), global, *value)?;
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

fn ensure_active_transaction(store: &mut dyn VMStore) -> Result<()> {
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
) -> Result<MemoryIndex> {
    let defined = DefinedMemoryIndex::from_u32(memory);
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let memory_index = instance_ref.env_module().memory_index(defined);
    ensure!(
        instance_ref.get_tmemory(memory_index).is_some(),
        "transactional memory operation targeted non-transactional memory"
    );
    Ok(memory_index)
}

fn grow_tmemory_to_pages(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory: u32,
    new_pages: u64,
) -> Result<()> {
    let runtime = store.store_opaque().transaction_region_runtime().clone();
    runtime.with_shared_file_backed_tmemory_commit_write_lock(|| {
        let memory = MemoryIndex::from_u32(memory);
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
            .context("transactional memory grow failed during commit")
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
    if let Some(size) = store
        .store_opaque_mut()
        .transaction_state_mut()
        .staged_table_size_owned(Some(instance), table)
    {
        return Ok(size);
    }
    u64::try_from(defined_table_size(store, instance, table)?)
        .context("defined table size does not fit u64")
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
) -> Result<(MemoryIndex, TMemoryAccessSnapshot)> {
    let memory_index = resolve_defined_tmemory_index(store, instance, memory)?;
    let snapshot = collect_tmemory_snapshot(store, instance, memory_index, addr, len)?;
    Ok((memory_index, snapshot))
}

fn collect_tmemory_snapshot(
    store: &mut dyn VMStore,
    instance: InstanceId,
    memory_index: MemoryIndex,
    addr: u64,
    len: usize,
) -> Result<TMemoryAccessSnapshot> {
    let instance_ref = store.instance_mut(instance);
    let instance_ref = instance_ref.as_ref();
    let tmemory = instance_ref
        .get_tmemory(memory_index)
        .context("transactional memory operation targeted non-transactional memory")?;
    collect_tmemory_access_snapshot(tmemory, addr, len)
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
            let memory_index = MemoryIndex::from_u32(memory_index);
            let instance_ref = store.instance_mut(owner);
            let instance_ref = instance_ref.as_ref();
            let tmemory = instance_ref
                .get_tmemory(memory_index)
                .context("transactional memory operation targeted non-transactional memory")?;
            tmemory.granule_version(
                usize::try_from(granule_index)
                    .context("tmemory granule index does not fit host usize")?,
            )
        }
        GranuleId::Object { object_id } => store
            .store_opaque_mut()
            .transaction_object_table()
            .version(object_id),
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
        OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT, ObjectId, ObjectKind,
        encode_object_record_for_recovery, type_layout::TypeLayoutRegistry,
    };

    fn recovered_struct_winner(
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
        crate::runtime::vm::RecoveredObjectWinner {
            object_id: object_id.object_index,
            version,
            kind: ObjectKind::Struct as u16,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::DEFAULT_STRUCT
                .get(),
            data_block: 0,
            data_offset: 0,
            record_len: record_bytes.len() as u64,
            record_bytes,
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
        objects
            .rebuild_reachable_from_recovered_object_winners(
                &TypeLayoutRegistry::default(),
                &[
                    recovered_struct_winner(child, 1, vec![ObjectValue::I32(9)]),
                    recovered_struct_winner(root, 1, vec![ObjectValue::Ref(Some(child))]),
                ],
                &[root.object_index],
            )
            .unwrap();
        let expected_handle = objects.transaction_ref_handle_for_object_id(child).unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();

        let abi = live_transaction_abi_from_object_value(
            &mut durable_refs,
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
            object_value_from_transaction_abi(&mut durable_refs, &mut objects, abi).unwrap(),
            ObjectValue::Ref(Some(child))
        );
    }

    #[test]
    fn object_value_abi_roundtrips_recovered_persistent_object_ref_without_gc_ref() {
        let root = ObjectId { object_index: 41 };
        let child = ObjectId { object_index: 42 };
        let mut objects = ObjectTable::default();
        objects
            .rebuild_reachable_from_recovered_object_winners(
                &TypeLayoutRegistry::default(),
                &[
                    recovered_struct_winner(child, 1, vec![ObjectValue::I32(9)]),
                    recovered_struct_winner(root, 1, vec![ObjectValue::Ref(Some(child))]),
                ],
                &[root.object_index],
            )
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let expected_handle = objects.transaction_ref_handle_for_object_id(child).unwrap();

        let abi = live_transaction_abi_from_object_value(
            &mut durable_refs,
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
            object_value_from_transaction_abi(&mut durable_refs, &mut objects, abi).unwrap(),
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

        let error = object_value_from_transaction_abi(&mut durable_refs, &mut objects, abi)
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

        let error = object_value_from_transaction_abi(&mut durable_refs, &mut objects, abi)
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
        objects
            .rebuild_reachable_from_recovered_object_winners(
                &TypeLayoutRegistry::default(),
                &[recovered_struct_winner(
                    object,
                    1,
                    vec![ObjectValue::I32(7)],
                )],
                &[object.object_index],
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

        let abi = live_transaction_abi_from_object_value(
            &durable_refs,
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
            objects
                .object_id_for_transaction_ref_handle(handle)
                .unwrap(),
            ObjectId { object_index: 0 }
        );
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
        let error = live_transaction_abi_from_object_value(
            &empty_refs,
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
    fn live_ref_bridge_rejects_untyped_gc_raw_that_only_matches_persistent_object_id() {
        let object = ObjectId { object_index: 41 };
        let gc_raw = object.object_index + 1;
        assert!(!ObjectTable::is_raw_i31_ref(gc_raw));

        let mut objects = ObjectTable::default();
        objects
            .rebuild_reachable_from_recovered_object_winners(
                &TypeLayoutRegistry::default(),
                &[recovered_struct_winner(
                    object,
                    1,
                    vec![ObjectValue::I32(7)],
                )],
                &[object.object_index],
            )
            .unwrap();
        let mut durable_refs = DurableReferenceRegistry::default();
        let handle = objects
            .transaction_ref_handle_for_object_id(object)
            .unwrap();

        let err = live_ref_value_from_raw(
            &mut durable_refs,
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
