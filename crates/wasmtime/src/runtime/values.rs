#[cfg(feature = "transaction")]
use crate::store::StoreId;
use crate::store::{AutoAssertNoGc, StoreOpaque};
use crate::{
    AnyRef, ArrayRef, AsContext, AsContextMut, ExnRef, ExternRef, Func, HeapType, RefType, Rooted,
    StructRef, V128, ValType, prelude::*,
};
use core::ptr;

pub use crate::runtime::vm::ValRaw;

/// A stub implementation for continuation references.
///
/// This is a placeholder until continuation objects are fully integrated
/// with the GC system (see #10248).
#[derive(Debug, Clone, Copy)]
pub struct ContRef;

/// Internal embedder representation for a transactional Wasm reference in
/// the `tany` hierarchy.
///
/// This type is public only because it is carried by the public [`Val`] enum;
/// transactional external and function references are separate inline `Val`
/// variants so `Val` retains its existing size. The contents are intentionally
/// opaque: transactional references originate from typed Wasm boundaries and
/// cannot be forged from integer bits.
#[cfg(feature = "transaction")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct TransactionRef {
    store: Option<StoreId>,
    inner: TransactionRefInner,
}

#[cfg(feature = "transaction")]
#[derive(Debug, Clone, Copy)]
enum TransactionRefInner {
    Null,
    Object(crate::runtime::transaction::TransactionObjectRefRaw),
    I31(u32),
}

#[cfg(feature = "transaction")]
impl TransactionRef {
    fn null(ref_ty: &RefType) -> Self {
        debug_assert!(ref_ty.is_transactional_ref());
        debug_assert!(matches!(ref_ty.heap_type().top(), HeapType::Any));
        Self {
            store: None,
            inner: TransactionRefInner::Null,
        }
    }

    unsafe fn from_raw(store: &mut AutoAssertNoGc<'_>, raw: ValRaw, ref_ty: &RefType) -> Val {
        debug_assert!(ref_ty.is_transactional_ref());
        match ref_ty.heap_type().top() {
            HeapType::Func => {
                let raw = raw.get_funcref();
                Val::TransactionFuncRef(match core::ptr::NonNull::new(raw) {
                    Some(raw) => Some(unsafe { Func::from_vm_func_ref(store.id(), raw.cast()) }),
                    None => None,
                })
            }
            HeapType::Extern => {
                let raw = raw.get_externref();
                Val::TransactionExternRef(ExternRef::_from_raw(store, raw))
            }
            HeapType::Any => {
                let raw = raw.get_anyref();
                let inner = if raw == 0 {
                    TransactionRefInner::Null
                } else if let Some(reference) = store.transaction_extern_for_handle(raw) {
                    return Val::TransactionExternRef(Some(reference));
                } else if crate::runtime::transaction::ObjectTable::is_raw_i31_ref(u64::from(raw)) {
                    TransactionRefInner::I31(raw)
                } else if store
                    .transaction_state()
                    .known_object_id_for_transaction_ref_handle(
                        store.transaction_object_table(),
                        raw,
                    )
                    .is_some()
                {
                    TransactionRefInner::Object(
                        crate::runtime::transaction::TransactionObjectRefRaw::from_raw(raw),
                    )
                } else {
                    return Val::TransactionExternRef(Some(
                        ExternRef::_from_raw(store, raw)
                            .expect("non-object transactional anyref must be an external identity"),
                    ));
                };
                Val::TransactionRef(Self {
                    store: (!matches!(inner, TransactionRefInner::Null)).then(|| store.id()),
                    inner,
                })
            }
            other => unreachable!("unsupported transactional reference hierarchy: {other}"),
        }
    }

    fn to_raw(self, store: &mut AutoAssertNoGc<'_>) -> Result<ValRaw> {
        ensure!(
            self.store.is_none_or(|id| id == store.id()),
            "transactional reference used with wrong store"
        );
        Ok(match self.inner {
            TransactionRefInner::Null => ValRaw::null(),
            TransactionRefInner::Object(raw) => ValRaw::anyref(raw.as_raw()),
            TransactionRefInner::I31(raw) => ValRaw::anyref(raw),
        })
    }

    fn actual_heap_type(self, store: &StoreOpaque) -> Result<HeapType> {
        ensure!(
            self.store.is_none_or(|id| id == store.id()),
            "transactional reference used with wrong store"
        );
        Ok(match self.inner {
            TransactionRefInner::Null => HeapType::None,
            TransactionRefInner::I31(_) => HeapType::I31,
            TransactionRefInner::Object(raw) => {
                let object_table = store.transaction_object_table();
                let transaction_state = store.transaction_state();
                let object_id = transaction_state
                    .known_object_id_for_transaction_ref_handle(object_table, raw.as_raw())
                    .context("unknown transaction object reference handle")?;
                match (
                    transaction_state.object_kind(object_table, object_id)?,
                    transaction_state.runtime_type_index(object_table, object_id)?,
                ) {
                    (crate::runtime::transaction::ObjectKind::Struct, Some(index)) => {
                        HeapType::ConcreteStruct(crate::StructType::from_shared_type_index(
                            store.engine(),
                            index,
                        ))
                    }
                    (crate::runtime::transaction::ObjectKind::Struct, None) => HeapType::Struct,
                    (crate::runtime::transaction::ObjectKind::Array, Some(index)) => {
                        HeapType::ConcreteArray(crate::ArrayType::from_shared_type_index(
                            store.engine(),
                            index,
                        ))
                    }
                    (crate::runtime::transaction::ObjectKind::Array, None) => HeapType::Array,
                    (crate::runtime::transaction::ObjectKind::I31, _) => HeapType::I31,
                    (crate::runtime::transaction::ObjectKind::Extern, _) => HeapType::Extern,
                    (crate::runtime::transaction::ObjectKind::Func, Some(index)) => {
                        HeapType::ConcreteFunc(crate::FuncType::from_shared_type_index(
                            store.engine(),
                            index,
                        ))
                    }
                    (crate::runtime::transaction::ObjectKind::Func, None) => HeapType::Func,
                }
            }
        })
    }

    fn matches_ty(self, store: &StoreOpaque, ref_ty: &RefType) -> Result<bool> {
        if !ref_ty.is_transactional_ref()
            || self.store.is_some_and(|id| id != store.id())
            || matches!(self.inner, TransactionRefInner::Null) && !ref_ty.is_nullable()
        {
            return Ok(false);
        }
        Ok(self.actual_heap_type(store)?.matches(ref_ty.heap_type()))
    }

    pub(crate) fn object_raw(self) -> Option<u32> {
        match self.inner {
            TransactionRefInner::Object(raw) => Some(raw.as_raw()),
            _ => None,
        }
    }

    pub(crate) fn is_i31(self) -> bool {
        matches!(self.inner, TransactionRefInner::I31(_))
    }

    #[cfg(test)]
    pub(crate) fn i31_value(self) -> Option<i32> {
        match self.inner {
            TransactionRefInner::I31(raw) => {
                crate::runtime::transaction::ObjectTable::decode_raw_i31_ref(u64::from(raw)).ok()
            }
            _ => None,
        }
    }

    pub(crate) fn is_null(self) -> bool {
        matches!(self.inner, TransactionRefInner::Null)
    }
}

/// A non-null reference in the transactional `tany` hierarchy.
#[cfg(feature = "transaction")]
#[derive(Debug, Clone, Copy)]
pub struct TransactionAnyRef(TransactionAnyRefInner);

#[cfg(feature = "transaction")]
#[derive(Debug, Clone, Copy)]
enum TransactionAnyRefInner {
    Any(TransactionRef),
    Extern(Rooted<ExternRef>),
}

#[cfg(feature = "transaction")]
impl TransactionAnyRef {
    /// Wraps an external reference converted into the transactional-any hierarchy.
    pub fn from_extern(reference: Rooted<ExternRef>) -> Self {
        Self(TransactionAnyRefInner::Extern(reference))
    }

    /// Returns the carried external reference, if this is a converted external identity.
    pub fn as_extern(&self) -> Option<Rooted<ExternRef>> {
        match self.0 {
            TransactionAnyRefInner::Extern(reference) => Some(reference),
            TransactionAnyRefInner::Any(_) => None,
        }
    }

    pub(crate) fn from_val(value: Val) -> Option<Self> {
        match value {
            Val::TransactionRef(reference) if !reference.is_null() => {
                Some(Self(TransactionAnyRefInner::Any(reference)))
            }
            Val::TransactionExternRef(Some(reference)) => {
                Some(Self(TransactionAnyRefInner::Extern(reference)))
            }
            _ => None,
        }
    }

    pub(crate) fn into_val(self) -> Val {
        match self.0 {
            TransactionAnyRefInner::Any(reference) => Val::TransactionRef(reference),
            TransactionAnyRefInner::Extern(reference) => Val::TransactionExternRef(Some(reference)),
        }
    }

    pub(crate) fn compatible_with_store(self, store: &StoreOpaque) -> bool {
        self.into_val().comes_from_same_store(store)
    }

    pub(crate) fn ensure_matches(self, store: &StoreOpaque, expected: &HeapType) -> Result<()> {
        let actual = match self.0 {
            TransactionAnyRefInner::Any(reference) => reference.actual_heap_type(store)?,
            TransactionAnyRefInner::Extern(_) => HeapType::Extern,
        };
        ensure!(
            actual.matches(expected),
            "argument type mismatch: transactional {actual} does not match {expected}"
        );
        Ok(())
    }

    pub(crate) fn is_vmgcref(self) -> bool {
        false
    }

    pub(crate) fn to_raw(self, store: &mut AutoAssertNoGc<'_>) -> Result<ValRaw> {
        match self.0 {
            TransactionAnyRefInner::Any(reference) => reference.to_raw(store),
            TransactionAnyRefInner::Extern(reference) => {
                let handle = store.transaction_extern_handle(reference)?;
                Ok(ValRaw::anyref(handle))
            }
        }
    }

    pub(crate) unsafe fn from_raw(store: &mut AutoAssertNoGc<'_>, raw: &ValRaw) -> Option<Self> {
        Self::from_val(unsafe {
            TransactionRef::from_raw(
                store,
                ValRaw::anyref(raw.get_anyref()),
                &RefType::new_transactional(false, HeapType::Any),
            )
        })
    }
}

/// A non-null transactional external reference.
#[cfg(feature = "transaction")]
#[derive(Debug, Clone, Copy)]
pub struct TransactionExternRef(Rooted<ExternRef>);

#[cfg(feature = "transaction")]
impl TransactionExternRef {
    /// Creates a transactional external reference from an ordinary rooted external identity.
    pub fn new(reference: Rooted<ExternRef>) -> Self {
        Self(reference)
    }

    /// Creates a transactional external reference with a durable host identity.
    ///
    /// Transactional references can be published into persistent tables,
    /// globals, and objects. To publish a host external reference, Wasmtime
    /// must be able to encode an identity that the embedder can bind again
    /// after reopening the persistent state. `namespace` identifies the
    /// embedder-defined class of handles and `handle` identifies this external
    /// reference within that namespace.
    ///
    /// Reopening persistent state in a new [`Store`][crate::Store] requires the
    /// embedder to call this constructor again with the same `namespace` and
    /// `handle` for the replacement live reference before Wasm reads the
    /// persisted value. The host data on `reference` is preserved.
    ///
    /// It is an error to bind one live reference to multiple durable
    /// identities, or multiple live references in the same store to one
    /// durable identity.
    pub fn new_durable(
        mut store: impl AsContextMut,
        reference: Rooted<ExternRef>,
        namespace: u32,
        handle: u64,
    ) -> Result<Self> {
        let mut store = store.as_context_mut();
        ensure!(
            Self(reference).compatible_with_store(store.0),
            "transactional external reference used with wrong store"
        );
        let raw_gc_ref = reference.to_raw(&mut store)?;
        let identity = crate::runtime::transaction::DurableExternIdentity {
            namespace,
            handle,
            type_layout_id: crate::runtime::transaction::type_layout::TypeLayoutId::BUILTIN_EXTERN,
        };
        store
            .0
            .transaction_register_durable_extern_ref(raw_gc_ref, identity)?;
        Ok(Self(reference))
    }

    /// Returns the underlying rooted external identity.
    pub fn get(self) -> Rooted<ExternRef> {
        self.0
    }

    pub(crate) fn compatible_with_store(self, store: &StoreOpaque) -> bool {
        self.0.comes_from_same_store(store)
    }
}

/// A non-null transactional function reference.
#[cfg(feature = "transaction")]
#[derive(Debug, Clone, Copy)]
pub struct TransactionFuncRef(Func);

#[cfg(feature = "transaction")]
impl TransactionFuncRef {
    /// Creates a transactional function reference.
    pub fn new(func: Func) -> Self {
        Self(func)
    }

    /// Returns the underlying function identity.
    pub fn get(self) -> Func {
        self.0
    }

    pub(crate) fn compatible_with_store(self, store: &StoreOpaque) -> bool {
        self.0.comes_from_same_store(store)
    }
}

/// Possible runtime values that a WebAssembly module can either consume or
/// produce.
///
/// Note that we inline the `enum Ref { ... }` variants into `enum Val { ... }`
/// here as a size optimization.
#[derive(Debug, Clone, Copy)]
pub enum Val {
    // NB: the ordering here is intended to match the ordering in
    // `ValType` to improve codegen when learning the type of a value.
    //
    /// A 32-bit integer.
    I32(i32),

    /// A 64-bit integer.
    I64(i64),

    /// A 32-bit float.
    ///
    /// Note that the raw bits of the float are stored here, and you can use
    /// `f32::from_bits` to create an `f32` value.
    F32(u32),

    /// A 64-bit float.
    ///
    /// Note that the raw bits of the float are stored here, and you can use
    /// `f64::from_bits` to create an `f64` value.
    F64(u64),

    /// A 128-bit number.
    V128(V128),

    /// A function reference.
    FuncRef(Option<Func>),

    /// An external reference.
    ExternRef(Option<Rooted<ExternRef>>),

    /// An internal reference.
    AnyRef(Option<Rooted<AnyRef>>),

    /// An exception reference.
    ExnRef(Option<Rooted<ExnRef>>),

    /// A continuation reference.
    ///
    /// Note: This is currently a stub implementation as continuation objects
    /// are not yet fully integrated with the GC system. See #10248.
    ContRef(Option<ContRef>),

    /// A typed transactional internal reference.
    #[cfg(feature = "transaction")]
    #[doc(hidden)]
    TransactionRef(TransactionRef),

    /// A typed transactional external reference.
    #[cfg(feature = "transaction")]
    #[doc(hidden)]
    TransactionExternRef(Option<Rooted<ExternRef>>),

    /// A typed transactional function reference.
    #[cfg(feature = "transaction")]
    #[doc(hidden)]
    TransactionFuncRef(Option<Func>),
}

macro_rules! accessors {
    ($bind:ident $(($variant:ident($ty:ty) $get:ident $unwrap:ident $cvt:expr))*) => ($(
        /// Attempt to access the underlying value of this `Val`, returning
        /// `None` if it is not the correct type.
        #[inline]
        pub fn $get(&self) -> Option<$ty> {
            if let Val::$variant($bind) = self {
                Some($cvt)
            } else {
                None
            }
        }

        /// Returns the underlying value of this `Val`, panicking if it's the
        /// wrong type.
        ///
        /// # Panics
        ///
        /// Panics if `self` is not of the right type.
        #[inline]
        pub fn $unwrap(&self) -> $ty {
            self.$get().expect(concat!("expected ", stringify!($ty)))
        }
    )*)
}

impl Val {
    /// Returns the null reference for the given heap type.
    #[inline]
    pub fn null_ref(heap_type: &HeapType) -> Val {
        Ref::null(&heap_type).into()
    }

    /// Returns the null function reference value.
    ///
    /// The return value has type `(ref null nofunc)` aka `nullfuncref` and is a
    /// subtype of all function references.
    #[inline]
    pub const fn null_func_ref() -> Val {
        Val::FuncRef(None)
    }

    /// Returns the null function reference value.
    ///
    /// The return value has type `(ref null extern)` aka `nullexternref` and is
    /// a subtype of all external references.
    #[inline]
    pub const fn null_extern_ref() -> Val {
        Val::ExternRef(None)
    }

    /// Returns the null function reference value.
    ///
    /// The return value has type `(ref null any)` aka `nullref` and is a
    /// subtype of all internal references.
    #[inline]
    pub const fn null_any_ref() -> Val {
        Val::AnyRef(None)
    }

    /// Returns the default value for the given type, if any exists.
    ///
    /// Returns `None` if there is no default value for the given type (for
    /// example, non-nullable reference types do not have a default value).
    pub fn default_for_ty(ty: &ValType) -> Option<Val> {
        match ty {
            ValType::I32 => Some(Val::I32(0)),
            ValType::I64 => Some(Val::I64(0)),
            ValType::F32 => Some(Val::F32(0)),
            ValType::F64 => Some(Val::F64(0)),
            ValType::V128 => Some(Val::V128(V128::from(0))),
            ValType::Ref(ref_ty) => {
                if ref_ty.is_nullable() {
                    #[cfg(feature = "transaction")]
                    if ref_ty.is_transactional_ref() {
                        return Some(match ref_ty.heap_type().top() {
                            HeapType::Any => Val::TransactionRef(TransactionRef::null(ref_ty)),
                            HeapType::Extern => Val::TransactionExternRef(None),
                            HeapType::Func => Val::TransactionFuncRef(None),
                            other => unreachable!(
                                "unsupported transactional reference hierarchy: {other}"
                            ),
                        });
                    }
                    Some(Val::null_ref(ref_ty.heap_type()))
                } else {
                    None
                }
            }
        }
    }

    /// Returns the corresponding [`ValType`] for this `Val`.
    ///
    /// # Errors
    ///
    /// Returns an error if this value is a GC reference that has since been
    /// unrooted.
    ///
    /// # Panics
    ///
    /// Panics if this value is associated with a different store.
    #[inline]
    pub fn ty(&self, store: impl AsContext) -> Result<ValType> {
        self.load_ty(&store.as_context().0)
    }

    #[inline]
    pub(crate) fn load_ty(&self, store: &StoreOpaque) -> Result<ValType> {
        Ok(match self {
            Val::I32(_) => ValType::I32,
            Val::I64(_) => ValType::I64,
            Val::F32(_) => ValType::F32,
            Val::F64(_) => ValType::F64,
            Val::V128(_) => ValType::V128,
            Val::ExternRef(Some(_)) => ValType::EXTERNREF,
            Val::ExternRef(None) => ValType::NULLFUNCREF,
            Val::FuncRef(None) => ValType::NULLFUNCREF,
            Val::FuncRef(Some(f)) => ValType::Ref(RefType::new(
                false,
                HeapType::ConcreteFunc(f.load_ty(store)),
            )),
            Val::AnyRef(None) => ValType::NULLREF,
            Val::AnyRef(Some(a)) => ValType::Ref(RefType::new(false, a._ty(store)?)),
            Val::ExnRef(None) => ValType::NULLEXNREF,
            Val::ExnRef(Some(e)) => ValType::Ref(RefType::new(false, e._ty(store)?.into())),
            Val::ContRef(_) => {
                // TODO(#10248): Return proper continuation reference type when available
                return Err(crate::format_err!(
                    "continuation references not yet supported in embedder API"
                ));
            }
            #[cfg(feature = "transaction")]
            Val::TransactionRef(reference) => ValType::Ref(RefType::new_transactional(
                matches!(reference.inner, TransactionRefInner::Null),
                reference.actual_heap_type(store)?,
            )),
            #[cfg(feature = "transaction")]
            Val::TransactionExternRef(reference) => ValType::Ref(RefType::new_transactional(
                reference.is_none(),
                if reference.is_some() {
                    HeapType::Extern
                } else {
                    HeapType::NoExtern
                },
            )),
            #[cfg(feature = "transaction")]
            Val::TransactionFuncRef(reference) => ValType::Ref(RefType::new_transactional(
                reference.is_none(),
                match reference {
                    Some(func) => HeapType::ConcreteFunc(func.load_ty(store)),
                    None => HeapType::NoFunc,
                },
            )),
        })
    }

    /// Does this value match the given type?
    ///
    /// Returns an error is an underlying `Rooted` has been unrooted.
    ///
    /// # Panics
    ///
    /// Panics if this value is not associated with the given store.
    pub fn matches_ty(&self, store: impl AsContext, ty: &ValType) -> Result<bool> {
        self._matches_ty(&store.as_context().0, ty)
    }

    pub(crate) fn _matches_ty(&self, store: &StoreOpaque, ty: &ValType) -> Result<bool> {
        assert!(self.comes_from_same_store(store));
        assert!(ty.comes_from_same_engine(store.engine()));
        #[cfg(feature = "transaction")]
        if matches!(ty, ValType::Ref(ref_ty) if ref_ty.is_transactional_ref())
            && !matches!(
                self,
                Val::TransactionRef(_) | Val::TransactionExternRef(_) | Val::TransactionFuncRef(_)
            )
        {
            return Ok(false);
        }
        Ok(match (self, ty) {
            (Val::I32(_), ValType::I32)
            | (Val::I64(_), ValType::I64)
            | (Val::F32(_), ValType::F32)
            | (Val::F64(_), ValType::F64)
            | (Val::V128(_), ValType::V128) => true,

            (Val::FuncRef(f), ValType::Ref(ref_ty)) => Ref::from(*f)._matches_ty(store, ref_ty)?,
            (Val::ExternRef(e), ValType::Ref(ref_ty)) => {
                Ref::from(*e)._matches_ty(store, ref_ty)?
            }
            (Val::AnyRef(a), ValType::Ref(ref_ty)) => Ref::from(*a)._matches_ty(store, ref_ty)?,
            (Val::ExnRef(e), ValType::Ref(ref_ty)) => Ref::from(*e)._matches_ty(store, ref_ty)?,
            #[cfg(feature = "transaction")]
            (Val::TransactionRef(reference), ValType::Ref(ref_ty)) => {
                reference.matches_ty(store, ref_ty)?
            }
            #[cfg(feature = "transaction")]
            (Val::TransactionExternRef(reference), ValType::Ref(ref_ty)) => {
                ref_ty.is_transactional_ref()
                    && reference.is_none_or(|reference| reference.comes_from_same_store(store))
                    && (reference.is_some() || ref_ty.is_nullable())
                    && match ref_ty.heap_type() {
                        HeapType::Extern => true,
                        // `tany.convert_textern` preserves the external
                        // identity while moving it into the transaction-any
                        // hierarchy.
                        HeapType::Any => reference.is_some(),
                        HeapType::NoExtern => reference.is_none(),
                        _ => false,
                    }
            }
            #[cfg(feature = "transaction")]
            (Val::TransactionFuncRef(reference), ValType::Ref(ref_ty)) => {
                ref_ty.is_transactional_ref()
                    && reference.is_none_or(|reference| reference.comes_from_same_store(store))
                    && (reference.is_some() || ref_ty.is_nullable())
                    && match reference {
                        Some(func) => {
                            HeapType::ConcreteFunc(func.load_ty(store)).matches(ref_ty.heap_type())
                        }
                        None => HeapType::NoFunc.matches(ref_ty.heap_type()),
                    }
            }

            (Val::I32(_), _)
            | (Val::I64(_), _)
            | (Val::F32(_), _)
            | (Val::F64(_), _)
            | (Val::V128(_), _)
            | (Val::FuncRef(_), _)
            | (Val::ExternRef(_), _)
            | (Val::AnyRef(_), _)
            | (Val::ExnRef(_), _)
            | (Val::ContRef(_), _) => false,
            #[cfg(feature = "transaction")]
            (Val::TransactionRef(_), _)
            | (Val::TransactionExternRef(_), _)
            | (Val::TransactionFuncRef(_), _) => false,
        })
    }

    pub(crate) fn ensure_matches_ty(&self, store: &StoreOpaque, ty: &ValType) -> Result<()> {
        if !self.comes_from_same_store(store) {
            bail!("value used with wrong store")
        }
        if !ty.comes_from_same_engine(store.engine()) {
            bail!("type used with wrong engine")
        }
        if self._matches_ty(store, ty)? {
            Ok(())
        } else {
            let actual_ty = self.load_ty(store)?;
            bail!("type mismatch: expected {ty}, found {actual_ty}")
        }
    }

    /// Convenience method to convert this [`Val`] into a [`ValRaw`].
    ///
    /// Returns an error if this value is a GC reference and the GC reference
    /// has been unrooted.
    ///
    /// # Safety
    ///
    /// The returned [`ValRaw`] does not carry type information and is only safe
    /// to use within the context of this store itself. For more information see
    /// [`ExternRef::to_raw`] and [`Func::to_raw`].
    pub fn to_raw(&self, mut store: impl AsContextMut) -> Result<ValRaw> {
        let mut store = AutoAssertNoGc::new(store.as_context_mut().0);
        self.to_raw_(&mut store)
    }

    pub(crate) fn to_raw_(&self, store: &mut AutoAssertNoGc) -> Result<ValRaw> {
        match self {
            Val::I32(i) => Ok(ValRaw::i32(*i)),
            Val::I64(i) => Ok(ValRaw::i64(*i)),
            Val::F32(u) => Ok(ValRaw::f32(*u)),
            Val::F64(u) => Ok(ValRaw::f64(*u)),
            Val::V128(b) => Ok(ValRaw::v128(b.as_u128())),
            Val::ExternRef(e) => Ok(ValRaw::externref(match e {
                None => 0,
                Some(e) => e._to_raw(store)?,
            })),
            Val::AnyRef(e) => Ok(ValRaw::anyref(match e {
                None => 0,
                Some(e) => e._to_raw(store)?,
            })),
            Val::ExnRef(e) => Ok(ValRaw::exnref(match e {
                None => 0,
                Some(e) => e._to_raw(store)?,
            })),
            Val::FuncRef(f) => Ok(ValRaw::funcref(match f {
                Some(f) => f.to_raw_(store),
                None => ptr::null_mut(),
            })),
            Val::ContRef(_) => {
                // TODO(#10248): Implement proper continuation reference to_raw conversion
                Err(crate::format_err!(
                    "continuation references not yet supported in to_raw conversion"
                ))
            }
            #[cfg(feature = "transaction")]
            Val::TransactionRef(_) | Val::TransactionExternRef(_) | Val::TransactionFuncRef(_) => {
                bail!(
                    "transactional references require a typed Wasm function boundary for raw conversion"
                )
            }
        }
    }

    /// Convenience method to convert a [`ValRaw`] into a [`Val`].
    ///
    /// # Unsafety
    ///
    /// This method is unsafe for the reasons that [`ExternRef::from_raw`] and
    /// [`Func::from_raw`] are unsafe. Additionally there's no guarantee
    /// otherwise that `raw` should have the type `ty` specified.
    pub unsafe fn from_raw(mut store: impl AsContextMut, raw: ValRaw, ty: ValType) -> Val {
        let store = store.as_context_mut().0;
        #[cfg(feature = "transaction")]
        if let Some(value) = unsafe { Self::transaction_ref_from_raw(store, raw, &ty) } {
            return value;
        }
        let mut store = AutoAssertNoGc::new(store);
        // SAFETY: `_from_raw` has the same contract as this function.
        unsafe { Self::_from_raw(&mut store, raw, &ty) }
    }

    /// Same as [`Self::from_raw`], but with a monomorphic store.
    pub(crate) unsafe fn _from_raw(
        store: &mut AutoAssertNoGc<'_>,
        raw: ValRaw,
        ty: &ValType,
    ) -> Val {
        #[cfg(feature = "transaction")]
        assert!(
            !matches!(ty, ValType::Ref(ref_ty) if ref_ty.is_transactional_ref()),
            "transactional references must be decoded before generic Val::from_raw"
        );
        match ty {
            ValType::I32 => Val::I32(raw.get_i32()),
            ValType::I64 => Val::I64(raw.get_i64()),
            ValType::F32 => Val::F32(raw.get_f32()),
            ValType::F64 => Val::F64(raw.get_f64()),
            ValType::V128 => Val::V128(raw.get_v128().into()),
            ValType::Ref(ref_ty) => {
                let ref_ = match ref_ty.heap_type() {
                    // SAFETY: it's a safety contract of this function that the
                    // funcref is valid and owned by the provided store.
                    HeapType::Func | HeapType::ConcreteFunc(_) => unsafe {
                        Func::_from_raw(store, raw.get_funcref()).into()
                    },

                    HeapType::NoFunc => Ref::Func(None),

                    HeapType::NoCont | HeapType::ConcreteCont(_) | HeapType::Cont => {
                        // TODO(#10248): Required to support stack switching in the embedder API.
                        unimplemented!()
                    }

                    HeapType::Extern => ExternRef::_from_raw(store, raw.get_externref()).into(),

                    HeapType::NoExtern => Ref::Extern(None),

                    HeapType::Any
                    | HeapType::Eq
                    | HeapType::I31
                    | HeapType::Array
                    | HeapType::ConcreteArray(_)
                    | HeapType::Struct
                    | HeapType::ConcreteStruct(_) => {
                        AnyRef::_from_raw(store, raw.get_anyref()).into()
                    }

                    HeapType::Exn | HeapType::ConcreteExn(_) => {
                        ExnRef::_from_raw(store, raw.get_exnref()).into()
                    }
                    HeapType::NoExn => Ref::Exn(None),

                    HeapType::None => Ref::Any(None),
                };
                assert!(
                    ref_ty.is_nullable() || !ref_.is_null(),
                    "if the type is not nullable, we shouldn't get null; got \
                     type = {ref_ty}, ref = {ref_:?}"
                );
                ref_.into()
            }
        }
    }

    #[cfg(feature = "transaction")]
    pub(crate) fn transaction_ref_to_raw(
        &self,
        store: &mut StoreOpaque,
        ty: &ValType,
    ) -> Result<Option<ValRaw>> {
        let ValType::Ref(ref_ty) = ty else {
            return Ok(None);
        };
        if !ref_ty.is_transactional_ref() {
            return Ok(None);
        }
        self.ensure_matches_ty(store, ty)?;
        let mut store = AutoAssertNoGc::new(store);
        Ok(Some(match self {
            Val::TransactionRef(reference) => reference.to_raw(&mut store)?,
            Val::TransactionExternRef(reference) => {
                let raw = match reference {
                    Some(reference) if matches!(ref_ty.heap_type().top(), HeapType::Any) => {
                        store.transaction_extern_handle(*reference)?
                    }
                    Some(reference) => reference._to_raw(&mut store)?,
                    None => 0,
                };
                ValRaw::externref(raw)
            }
            Val::TransactionFuncRef(reference) => ValRaw::funcref(match reference {
                Some(reference) => reference.to_raw_(&mut store),
                None => ptr::null_mut(),
            }),
            _ => {
                unreachable!("transactional reference type check accepted a non-transaction value")
            }
        }))
    }

    #[cfg(feature = "transaction")]
    pub(crate) unsafe fn transaction_ref_from_raw(
        store: &mut StoreOpaque,
        raw: ValRaw,
        ty: &ValType,
    ) -> Option<Val> {
        let ValType::Ref(ref_ty) = ty else {
            return None;
        };
        if !ref_ty.is_transactional_ref() {
            return None;
        }
        let mut store = AutoAssertNoGc::new(store);
        unsafe { Self::transaction_ref_from_raw_no_gc(&mut store, raw, ref_ty) }
    }

    #[cfg(feature = "transaction")]
    pub(crate) unsafe fn transaction_ref_from_raw_no_gc(
        store: &mut AutoAssertNoGc<'_>,
        raw: ValRaw,
        ref_ty: &RefType,
    ) -> Option<Val> {
        if !ref_ty.is_transactional_ref() {
            return None;
        }
        let reference = unsafe { TransactionRef::from_raw(store, raw, ref_ty) };
        assert!(
            reference
                ._matches_ty(store, &ValType::Ref(ref_ty.clone()))
                .unwrap(),
            "raw transactional reference does not match its function type"
        );
        Some(reference)
    }

    accessors! {
        e
        (I32(i32) i32 unwrap_i32 *e)
        (I64(i64) i64 unwrap_i64 *e)
        (F32(f32) f32 unwrap_f32 f32::from_bits(*e))
        (F64(f64) f64 unwrap_f64 f64::from_bits(*e))
        (FuncRef(Option<&Func>) func_ref unwrap_func_ref e.as_ref())
        (ExternRef(Option<&Rooted<ExternRef>>) extern_ref unwrap_extern_ref e.as_ref())
        (AnyRef(Option<&Rooted<AnyRef>>) any_ref unwrap_any_ref e.as_ref())
        (V128(V128) v128 unwrap_v128 *e)
    }

    /// Get this value's underlying reference, if any.
    #[inline]
    pub fn ref_(self) -> Option<Ref> {
        match self {
            Val::FuncRef(f) => Some(Ref::Func(f)),
            Val::ExternRef(e) => Some(Ref::Extern(e)),
            Val::AnyRef(a) => Some(Ref::Any(a)),
            Val::ExnRef(e) => Some(Ref::Exn(e)),
            Val::I32(_) | Val::I64(_) | Val::F32(_) | Val::F64(_) | Val::V128(_) => None,
            Val::ContRef(_) => None, // TODO(#10248): Return proper Ref::Cont when available
            #[cfg(feature = "transaction")]
            Val::TransactionRef(_) | Val::TransactionExternRef(_) | Val::TransactionFuncRef(_) => {
                None
            }
        }
    }

    /// Attempt to access the underlying `externref` value of this `Val`.
    ///
    /// If this is not an `externref`, then `None` is returned.
    ///
    /// If this is a null `externref`, then `Some(None)` is returned.
    ///
    /// If this is a non-null `externref`, then `Some(Some(..))` is returned.
    #[inline]
    pub fn externref(&self) -> Option<Option<&Rooted<ExternRef>>> {
        match self {
            Val::ExternRef(None) => Some(None),
            Val::ExternRef(Some(e)) => Some(Some(e)),
            _ => None,
        }
    }

    /// Returns the underlying `externref` value of this `Val`, panicking if it's the
    /// wrong type.
    ///
    /// If this is a null `externref`, then `None` is returned.
    ///
    /// If this is a non-null `externref`, then `Some(..)` is returned.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a (nullable) `externref`.
    #[inline]
    pub fn unwrap_externref(&self) -> Option<&Rooted<ExternRef>> {
        self.externref().expect("expected externref")
    }

    /// Attempt to access the underlying `anyref` value of this `Val`.
    ///
    /// If this is not an `anyref`, then `None` is returned.
    ///
    /// If this is a null `anyref`, then `Some(None)` is returned.
    ///
    /// If this is a non-null `anyref`, then `Some(Some(..))` is returned.
    #[inline]
    pub fn anyref(&self) -> Option<Option<&Rooted<AnyRef>>> {
        match self {
            Val::AnyRef(None) => Some(None),
            Val::AnyRef(Some(e)) => Some(Some(e)),
            _ => None,
        }
    }

    /// Returns the underlying `anyref` value of this `Val`, panicking if it's the
    /// wrong type.
    ///
    /// If this is a null `anyref`, then `None` is returned.
    ///
    /// If this is a non-null `anyref`, then `Some(..)` is returned.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a (nullable) `anyref`.
    #[inline]
    pub fn unwrap_anyref(&self) -> Option<&Rooted<AnyRef>> {
        self.anyref().expect("expected anyref")
    }

    /// Attempt to access the underlying `exnref` value of this `Val`.
    ///
    /// If this is not an `exnref`, then `None` is returned.
    ///
    /// If this is a null `exnref`, then `Some(None)` is returned.
    ///
    /// If this is a non-null `exnref`, then `Some(Some(..))` is returned.
    #[inline]
    pub fn exnref(&self) -> Option<Option<&Rooted<ExnRef>>> {
        match self {
            Val::ExnRef(None) => Some(None),
            Val::ExnRef(Some(e)) => Some(Some(e)),
            _ => None,
        }
    }

    /// Returns the underlying `exnref` value of this `Val`, panicking if it's the
    /// wrong type.
    ///
    /// If this is a null `exnref`, then `None` is returned.
    ///
    /// If this is a non-null `exnref`, then `Some(..)` is returned.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a (nullable) `exnref`.
    #[inline]
    pub fn unwrap_exnref(&self) -> Option<&Rooted<ExnRef>> {
        self.exnref().expect("expected exnref")
    }

    /// Attempt to access the underlying `funcref` value of this `Val`.
    ///
    /// If this is not an `funcref`, then `None` is returned.
    ///
    /// If this is a null `funcref`, then `Some(None)` is returned.
    ///
    /// If this is a non-null `funcref`, then `Some(Some(..))` is returned.
    #[inline]
    pub fn funcref(&self) -> Option<Option<&Func>> {
        match self {
            Val::FuncRef(None) => Some(None),
            Val::FuncRef(Some(f)) => Some(Some(f)),
            _ => None,
        }
    }

    /// Returns the underlying `funcref` value of this `Val`, panicking if it's the
    /// wrong type.
    ///
    /// If this is a null `funcref`, then `None` is returned.
    ///
    /// If this is a non-null `funcref`, then `Some(..)` is returned.
    ///
    /// # Panics
    ///
    /// Panics if `self` is not a (nullable) `funcref`.
    #[inline]
    pub fn unwrap_funcref(&self) -> Option<&Func> {
        self.funcref().expect("expected funcref")
    }

    #[inline]
    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        match self {
            Val::FuncRef(Some(f)) => f.comes_from_same_store(store),
            Val::FuncRef(None) => true,

            Val::ExternRef(Some(x)) => x.comes_from_same_store(store),
            Val::ExternRef(None) => true,

            Val::AnyRef(Some(a)) => a.comes_from_same_store(store),
            Val::AnyRef(None) => true,

            Val::ExnRef(Some(e)) => e.comes_from_same_store(store),
            Val::ExnRef(None) => true,

            // Integers, floats, and vectors have no association with any
            // particular store, so they're always considered as "yes I came
            // from that store",
            Val::I32(_) | Val::I64(_) | Val::F32(_) | Val::F64(_) | Val::V128(_) => true,

            // Continuation references are not yet associated with stores
            Val::ContRef(_) => true, // TODO(#10248): Proper store association when implemented

            #[cfg(feature = "transaction")]
            Val::TransactionRef(reference) => reference.store.is_none_or(|id| id == store.id()),
            #[cfg(feature = "transaction")]
            Val::TransactionExternRef(reference) => reference
                .as_ref()
                .is_none_or(|reference| reference.comes_from_same_store(store)),
            #[cfg(feature = "transaction")]
            Val::TransactionFuncRef(reference) => reference
                .as_ref()
                .is_none_or(|reference| reference.comes_from_same_store(store)),
        }
    }
}

impl From<i32> for Val {
    #[inline]
    fn from(val: i32) -> Val {
        Val::I32(val)
    }
}

impl From<i64> for Val {
    #[inline]
    fn from(val: i64) -> Val {
        Val::I64(val)
    }
}

impl From<f32> for Val {
    #[inline]
    fn from(val: f32) -> Val {
        Val::F32(val.to_bits())
    }
}

impl From<f64> for Val {
    #[inline]
    fn from(val: f64) -> Val {
        Val::F64(val.to_bits())
    }
}

impl From<Ref> for Val {
    #[inline]
    fn from(val: Ref) -> Val {
        match val {
            Ref::Extern(e) => Val::ExternRef(e),
            Ref::Func(f) => Val::FuncRef(f),
            Ref::Any(a) => Val::AnyRef(a),
            Ref::Exn(e) => Val::ExnRef(e),
        }
    }
}

impl From<Rooted<ExternRef>> for Val {
    #[inline]
    fn from(val: Rooted<ExternRef>) -> Val {
        Val::ExternRef(Some(val))
    }
}

impl From<Option<Rooted<ExternRef>>> for Val {
    #[inline]
    fn from(val: Option<Rooted<ExternRef>>) -> Val {
        Val::ExternRef(val)
    }
}

impl From<Rooted<AnyRef>> for Val {
    #[inline]
    fn from(val: Rooted<AnyRef>) -> Val {
        Val::AnyRef(Some(val))
    }
}

impl From<Option<Rooted<AnyRef>>> for Val {
    #[inline]
    fn from(val: Option<Rooted<AnyRef>>) -> Val {
        Val::AnyRef(val)
    }
}

impl From<Rooted<StructRef>> for Val {
    #[inline]
    fn from(val: Rooted<StructRef>) -> Val {
        Val::AnyRef(Some(val.into()))
    }
}

impl From<Option<Rooted<StructRef>>> for Val {
    #[inline]
    fn from(val: Option<Rooted<StructRef>>) -> Val {
        Val::AnyRef(val.map(Into::into))
    }
}

impl From<Rooted<ArrayRef>> for Val {
    #[inline]
    fn from(val: Rooted<ArrayRef>) -> Val {
        Val::AnyRef(Some(val.into()))
    }
}

impl From<Option<Rooted<ArrayRef>>> for Val {
    #[inline]
    fn from(val: Option<Rooted<ArrayRef>>) -> Val {
        Val::AnyRef(val.map(Into::into))
    }
}

impl From<Rooted<ExnRef>> for Val {
    #[inline]
    fn from(val: Rooted<ExnRef>) -> Val {
        Val::ExnRef(Some(val))
    }
}

impl From<Option<Rooted<ExnRef>>> for Val {
    #[inline]
    fn from(val: Option<Rooted<ExnRef>>) -> Val {
        Val::ExnRef(val)
    }
}

impl From<Func> for Val {
    #[inline]
    fn from(val: Func) -> Val {
        Val::FuncRef(Some(val))
    }
}

impl From<Option<Func>> for Val {
    #[inline]
    fn from(val: Option<Func>) -> Val {
        Val::FuncRef(val)
    }
}

impl From<u128> for Val {
    #[inline]
    fn from(val: u128) -> Val {
        Val::V128(val.into())
    }
}

impl From<V128> for Val {
    #[inline]
    fn from(val: V128) -> Val {
        Val::V128(val)
    }
}

/// A reference.
///
/// References come in three broad flavors:
///
/// 1. Function references. These are references to a function that can be
///    invoked.
///
/// 2. External references. These are references to data that is external
///    and opaque to the Wasm guest, provided by the host.
///
/// 3. Internal references. These are references to allocations inside the
///    Wasm's heap, such as structs and arrays. These are part of the GC
///    proposal, and not yet implemented in Wasmtime.
///
/// At the Wasm level, there are nullable and non-nullable variants of each type
/// of reference. Both variants are represented with `Ref` at the Wasmtime API
/// level. For example, values of both `(ref extern)` and `(ref null extern)`
/// types will be represented as `Ref::Extern(Option<ExternRef>)` in the
/// Wasmtime API. Nullable references are represented as `Option<Ref>` where
/// null references are represented as `None`. Wasm can construct null
/// references via the `ref.null <heap-type>` instruction.
///
/// References are non-forgable: Wasm cannot create invalid references, for
/// example, by claiming that the integer `0xbad1bad2` is actually a reference.
#[derive(Debug, Clone)]
pub enum Ref {
    // NB: We have a variant for each of the type hierarchies defined in Wasm,
    // and push the `Option` that provides nullability into each variant. This
    // allows us to get the most-precise type of any reference value, whether it
    // is null or not, without any additional metadata.
    //
    // Consider if we instead had the nullability inside `Val::Ref` and each of
    // the `Ref` variants did not have an `Option`:
    //
    //     enum Val {
    //         Ref(Option<Ref>),
    //         // Etc...
    //     }
    //     enum Ref {
    //         Func(Func),
    //         External(ExternRef),
    //         // Etc...
    //     }
    //
    // In this scenario, what type would we return from `Val::ty` for
    // `Val::Ref(None)`? Because Wasm has multiple separate type hierarchies,
    // there is no single common bottom type for all the different kinds of
    // references. So in this scenario, `Val::Ref(None)` doesn't have enough
    // information to reconstruct the value's type. That's a problem for us
    // because we need to get a value's type at various times all over the code
    // base.
    //
    /// A first-class reference to a WebAssembly function.
    ///
    /// The host, or the Wasm guest, can invoke this function.
    ///
    /// The host can create function references via [`Func::new`] or
    /// [`Func::wrap`].
    ///
    /// The Wasm guest can create non-null function references via the
    /// `ref.func` instruction, or null references via the `ref.null func`
    /// instruction.
    Func(Option<Func>),

    /// A reference to an value outside of the Wasm heap.
    ///
    /// These references are opaque to the Wasm itself. Wasm can't create
    /// non-null external references, nor do anything with them accept pass them
    /// around as function arguments and returns and place them into globals and
    /// tables.
    ///
    /// Wasm can create null external references via the `ref.null extern`
    /// instruction.
    Extern(Option<Rooted<ExternRef>>),

    /// An internal reference.
    ///
    /// The `AnyRef` type represents WebAssembly `anyref` values. These can be
    /// references to `struct`s and `array`s or inline/unboxed 31-bit
    /// integers.
    ///
    /// Unlike `externref`, Wasm guests can directly allocate `anyref`s, and
    /// does not need to rely on the host to do that.
    Any(Option<Rooted<AnyRef>>),

    /// An exception-object reference.
    ///
    /// The `ExnRef` type represents WebAssembly `exnref`
    /// values. These are references to exception objects as caught by
    /// `catch_ref` clauses on `try_table` instructions, or as
    /// allocated via the host API.
    Exn(Option<Rooted<ExnRef>>),
}

impl From<Func> for Ref {
    #[inline]
    fn from(f: Func) -> Ref {
        Ref::Func(Some(f))
    }
}

impl From<Option<Func>> for Ref {
    #[inline]
    fn from(f: Option<Func>) -> Ref {
        Ref::Func(f)
    }
}

impl From<Rooted<ExternRef>> for Ref {
    #[inline]
    fn from(e: Rooted<ExternRef>) -> Ref {
        Ref::Extern(Some(e))
    }
}

impl From<Option<Rooted<ExternRef>>> for Ref {
    #[inline]
    fn from(e: Option<Rooted<ExternRef>>) -> Ref {
        Ref::Extern(e)
    }
}

impl From<Rooted<AnyRef>> for Ref {
    #[inline]
    fn from(e: Rooted<AnyRef>) -> Ref {
        Ref::Any(Some(e))
    }
}

impl From<Option<Rooted<AnyRef>>> for Ref {
    #[inline]
    fn from(e: Option<Rooted<AnyRef>>) -> Ref {
        Ref::Any(e)
    }
}

impl From<Rooted<StructRef>> for Ref {
    #[inline]
    fn from(e: Rooted<StructRef>) -> Ref {
        Ref::Any(Some(e.into()))
    }
}

impl From<Option<Rooted<StructRef>>> for Ref {
    #[inline]
    fn from(e: Option<Rooted<StructRef>>) -> Ref {
        Ref::Any(e.map(Into::into))
    }
}

impl From<Rooted<ArrayRef>> for Ref {
    #[inline]
    fn from(e: Rooted<ArrayRef>) -> Ref {
        Ref::Any(Some(e.into()))
    }
}

impl From<Option<Rooted<ArrayRef>>> for Ref {
    #[inline]
    fn from(e: Option<Rooted<ArrayRef>>) -> Ref {
        Ref::Any(e.map(Into::into))
    }
}

impl From<Rooted<ExnRef>> for Ref {
    #[inline]
    fn from(e: Rooted<ExnRef>) -> Ref {
        Ref::Exn(Some(e))
    }
}

impl From<Option<Rooted<ExnRef>>> for Ref {
    #[inline]
    fn from(e: Option<Rooted<ExnRef>>) -> Ref {
        Ref::Exn(e)
    }
}

impl Ref {
    /// Create a null reference to the given heap type.
    #[inline]
    pub fn null(heap_type: &HeapType) -> Self {
        match heap_type.top() {
            HeapType::Any => Ref::Any(None),
            HeapType::Extern => Ref::Extern(None),
            HeapType::Func => Ref::Func(None),
            HeapType::Exn => Ref::Exn(None),
            ty => unreachable!("not a heap type: {ty:?}"),
        }
    }

    /// Is this a null reference?
    #[inline]
    pub fn is_null(&self) -> bool {
        match self {
            Ref::Any(None) | Ref::Extern(None) | Ref::Func(None) | Ref::Exn(None) => true,
            Ref::Any(Some(_)) | Ref::Extern(Some(_)) | Ref::Func(Some(_)) | Ref::Exn(Some(_)) => {
                false
            }
        }
    }

    /// Is this a non-null reference?
    #[inline]
    pub fn is_non_null(&self) -> bool {
        !self.is_null()
    }

    /// Is this an `extern` reference?
    #[inline]
    pub fn is_extern(&self) -> bool {
        matches!(self, Ref::Extern(_))
    }

    /// Get the underlying `extern` reference, if any.
    ///
    /// Returns `None` if this `Ref` is not an `extern` reference, eg it is a
    /// `func` reference.
    ///
    /// Returns `Some(None)` if this `Ref` is a null `extern` reference.
    ///
    /// Returns `Some(Some(_))` if this `Ref` is a non-null `extern` reference.
    #[inline]
    pub fn as_extern(&self) -> Option<Option<&Rooted<ExternRef>>> {
        match self {
            Ref::Extern(e) => Some(e.as_ref()),
            _ => None,
        }
    }

    /// Get the underlying `extern` reference, panicking if this is a different
    /// kind of reference.
    ///
    /// Returns `None` if this `Ref` is a null `extern` reference.
    ///
    /// Returns `Some(_)` if this `Ref` is a non-null `extern` reference.
    #[inline]
    pub fn unwrap_extern(&self) -> Option<&Rooted<ExternRef>> {
        self.as_extern()
            .expect("Ref::unwrap_extern on non-extern reference")
    }

    /// Is this an `any` reference?
    #[inline]
    pub fn is_any(&self) -> bool {
        matches!(self, Ref::Any(_))
    }

    /// Get the underlying `any` reference, if any.
    ///
    /// Returns `None` if this `Ref` is not an `any` reference, eg it is a
    /// `func` reference.
    ///
    /// Returns `Some(None)` if this `Ref` is a null `any` reference.
    ///
    /// Returns `Some(Some(_))` if this `Ref` is a non-null `any` reference.
    #[inline]
    pub fn as_any(&self) -> Option<Option<&Rooted<AnyRef>>> {
        match self {
            Ref::Any(e) => Some(e.as_ref()),
            _ => None,
        }
    }

    /// Get the underlying `any` reference, panicking if this is a different
    /// kind of reference.
    ///
    /// Returns `None` if this `Ref` is a null `any` reference.
    ///
    /// Returns `Some(_)` if this `Ref` is a non-null `any` reference.
    #[inline]
    pub fn unwrap_any(&self) -> Option<&Rooted<AnyRef>> {
        self.as_any().expect("Ref::unwrap_any on non-any reference")
    }

    /// Is this an `exn` reference?
    #[inline]
    pub fn is_exn(&self) -> bool {
        matches!(self, Ref::Exn(_))
    }

    /// Get the underlying `exn` reference, if any.
    ///
    /// Returns `None` if this `Ref` is not an `exn` reference, eg it is a
    /// `func` reference.
    ///
    /// Returns `Some(None)` if this `Ref` is a null `exn` reference.
    ///
    /// Returns `Some(Some(_))` if this `Ref` is a non-null `exn` reference.
    #[inline]
    pub fn as_exn(&self) -> Option<Option<&Rooted<ExnRef>>> {
        match self {
            Ref::Exn(e) => Some(e.as_ref()),
            _ => None,
        }
    }

    /// Get the underlying `exn` reference, panicking if this is a different
    /// kind of reference.
    ///
    /// Returns `None` if this `Ref` is a null `exn` reference.
    ///
    /// Returns `Some(_)` if this `Ref` is a non-null `exn` reference.
    #[inline]
    pub fn unwrap_exn(&self) -> Option<&Rooted<ExnRef>> {
        self.as_exn().expect("Ref::unwrap_exn on non-exn reference")
    }

    /// Is this a `func` reference?
    #[inline]
    pub fn is_func(&self) -> bool {
        matches!(self, Ref::Func(_))
    }

    /// Get the underlying `func` reference, if any.
    ///
    /// Returns `None` if this `Ref` is not an `func` reference, eg it is an
    /// `extern` reference.
    ///
    /// Returns `Some(None)` if this `Ref` is a null `func` reference.
    ///
    /// Returns `Some(Some(_))` if this `Ref` is a non-null `func` reference.
    #[inline]
    pub fn as_func(&self) -> Option<Option<&Func>> {
        match self {
            Ref::Func(f) => Some(f.as_ref()),
            _ => None,
        }
    }

    /// Get the underlying `func` reference, panicking if this is a different
    /// kind of reference.
    ///
    /// Returns `None` if this `Ref` is a null `func` reference.
    ///
    /// Returns `Some(_)` if this `Ref` is a non-null `func` reference.
    #[inline]
    pub fn unwrap_func(&self) -> Option<&Func> {
        self.as_func()
            .expect("Ref::unwrap_func on non-func reference")
    }

    /// Get the type of this reference.
    ///
    /// # Errors
    ///
    /// Return an error if this reference has been unrooted.
    ///
    /// # Panics
    ///
    /// Panics if this reference is associated with a different store.
    pub fn ty(&self, store: impl AsContext) -> Result<RefType> {
        self.load_ty(&store.as_context().0)
    }

    pub(crate) fn load_ty(&self, store: &StoreOpaque) -> Result<RefType> {
        assert!(self.comes_from_same_store(store));
        Ok(RefType::new(
            self.is_null(),
            // NB: We choose the most-specific heap type we can here and let
            // subtyping do its thing if callers are matching against a
            // `HeapType::Func`.
            match self {
                Ref::Extern(None) => HeapType::NoExtern,
                Ref::Extern(Some(_)) => HeapType::Extern,

                Ref::Func(None) => HeapType::NoFunc,
                Ref::Func(Some(f)) => HeapType::ConcreteFunc(f.load_ty(store)),

                Ref::Any(None) => HeapType::None,
                Ref::Any(Some(a)) => a._ty(store)?,

                Ref::Exn(None) => HeapType::None,
                Ref::Exn(Some(e)) => e._ty(store)?.into(),
            },
        ))
    }

    /// Does this reference value match the given type?
    ///
    /// Returns an error if the underlying `Rooted` has been unrooted.
    ///
    /// # Panics
    ///
    /// Panics if this reference is not associated with the given store.
    pub fn matches_ty(&self, store: impl AsContext, ty: &RefType) -> Result<bool> {
        self._matches_ty(&store.as_context().0, ty)
    }

    pub(crate) fn _matches_ty(&self, store: &StoreOpaque, ty: &RefType) -> Result<bool> {
        assert!(self.comes_from_same_store(store));
        assert!(ty.comes_from_same_engine(store.engine()));
        if self.is_null() && !ty.is_nullable() {
            return Ok(false);
        }
        Ok(match (self, ty.heap_type()) {
            (Ref::Extern(_), HeapType::Extern) => true,
            (Ref::Extern(None), HeapType::NoExtern) => true,
            (Ref::Extern(_), _) => false,

            (Ref::Func(_), HeapType::Func) => true,
            (Ref::Func(None), HeapType::NoFunc | HeapType::ConcreteFunc(_)) => true,
            (Ref::Func(Some(f)), HeapType::ConcreteFunc(func_ty)) => f._matches_ty(store, func_ty),
            (Ref::Func(_), _) => false,

            (Ref::Any(_), HeapType::Any) => true,
            (Ref::Any(Some(a)), HeapType::I31) => a._is_i31(store)?,
            (Ref::Any(Some(a)), HeapType::Struct) => a._is_struct(store)?,
            (Ref::Any(Some(a)), HeapType::ConcreteStruct(_ty)) => match a._as_struct(store)? {
                None => false,
                Some(s) => s._matches_ty(store, _ty)?,
            },
            (Ref::Any(Some(a)), HeapType::Eq) => a._is_eqref(store)?,
            (Ref::Any(Some(a)), HeapType::Array) => a._is_array(store)?,
            (Ref::Any(Some(a)), HeapType::ConcreteArray(_ty)) => match a._as_array(store)? {
                None => false,
                Some(a) => a._matches_ty(store, _ty)?,
            },
            (
                Ref::Any(None),
                HeapType::None
                | HeapType::I31
                | HeapType::ConcreteStruct(_)
                | HeapType::Struct
                | HeapType::ConcreteArray(_)
                | HeapType::Array
                | HeapType::Eq,
            ) => true,
            (Ref::Any(_), _) => false,

            (Ref::Exn(_), HeapType::Exn) => true,
            (Ref::Exn(None), HeapType::NoExn | HeapType::ConcreteExn(_)) => true,
            (Ref::Exn(Some(e)), HeapType::ConcreteExn(_)) => {
                e._matches_ty(store, &ty.heap_type())?
            }
            (Ref::Exn(_), _) => false,
        })
    }

    pub(crate) fn ensure_matches_ty(&self, store: &StoreOpaque, ty: &RefType) -> Result<()> {
        if !self.comes_from_same_store(store) {
            bail!("reference used with wrong store")
        }
        if !ty.comes_from_same_engine(store.engine()) {
            bail!("type used with wrong engine")
        }
        if self._matches_ty(store, ty)? {
            Ok(())
        } else {
            let actual_ty = self.load_ty(store)?;
            bail!("type mismatch: expected {ty}, found {actual_ty}")
        }
    }

    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        match self {
            Ref::Func(Some(f)) => f.comes_from_same_store(store),
            Ref::Func(None) => true,
            Ref::Extern(Some(x)) => x.comes_from_same_store(store),
            Ref::Extern(None) => true,
            Ref::Any(Some(a)) => a.comes_from_same_store(store),
            Ref::Any(None) => true,
            Ref::Exn(Some(e)) => e.comes_from_same_store(store),
            Ref::Exn(None) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn size_of_val() {
        // Try to keep tabs on the size of `Val` and make sure we don't grow its
        // size.
        let expected = if cfg!(target_arch = "x86_64")
            || cfg!(target_arch = "aarch64")
            || cfg!(target_arch = "s390x")
            || cfg!(target_arch = "riscv64")
            || cfg!(target_arch = "arm")
        {
            24
        } else if cfg!(target_arch = "x86") {
            20
        } else {
            panic!("unsupported architecture")
        };
        assert_eq!(std::mem::size_of::<Val>(), expected);
    }

    #[test]
    fn size_of_ref() {
        // Try to keep tabs on the size of `Ref` and make sure we don't grow its
        // size.
        let expected = if cfg!(target_arch = "x86_64")
            || cfg!(target_arch = "aarch64")
            || cfg!(target_arch = "s390x")
            || cfg!(target_arch = "riscv64")
            || cfg!(target_arch = "arm")
        {
            24
        } else if cfg!(target_arch = "x86") {
            20
        } else {
            panic!("unsupported architecture")
        };
        assert_eq!(std::mem::size_of::<Ref>(), expected);
    }

    #[test]
    #[should_panic]
    fn val_matches_ty_wrong_engine() {
        let e1 = Engine::default();
        let e2 = Engine::default();

        let t1 = FuncType::new(&e1, None, None);
        let t2 = FuncType::new(&e2, None, None);

        let mut s1 = Store::new(&e1, ());
        let f = Func::new(&mut s1, t1.clone(), |_caller, _args, _results| Ok(()));

        // Should panic.
        let _ = Val::FuncRef(Some(f)).matches_ty(
            &s1,
            &ValType::Ref(RefType::new(true, HeapType::ConcreteFunc(t2))),
        );
    }

    #[test]
    #[should_panic]
    fn ref_matches_ty_wrong_engine() {
        let e1 = Engine::default();
        let e2 = Engine::default();

        let t1 = FuncType::new(&e1, None, None);
        let t2 = FuncType::new(&e2, None, None);

        let mut s1 = Store::new(&e1, ());
        let f = Func::new(&mut s1, t1.clone(), |_caller, _args, _results| Ok(()));

        // Should panic.
        let _ = Ref::Func(Some(f)).matches_ty(&s1, &RefType::new(true, HeapType::ConcreteFunc(t2)));
    }
}
