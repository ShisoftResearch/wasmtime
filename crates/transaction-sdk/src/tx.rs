use core::marker::PhantomData;

use crate::{PMut, PRef, Persist, Root};

pub struct Tx<'tx> {
    _marker: PhantomData<&'tx mut ()>,
}

impl<'tx> Tx<'tx> {
    pub unsafe fn from_marker() -> Self {
        Self {
            _marker: PhantomData,
        }
    }

    pub fn root_ref<'borrow, T: Persist>(&'borrow self, root: &Root<T>) -> PRef<'borrow, T> {
        PRef::from_id(root.id())
    }

    /// ```compile_fail
    /// use wasmtime_transaction_sdk::{PMut, Persist, Root, transaction};
    ///
    /// #[repr(C)]
    /// struct Counter {
    ///     value: u32,
    /// }
    ///
    /// unsafe impl Persist for Counter {
    ///     const TYPE_NAME: &'static str = "Counter";
    /// }
    ///
    /// let root = Root::<Counter>::tmemory_offset(0);
    /// let leaked: PMut<'static, Counter> = transaction!(tx, { tx.root_mut(&root) });
    /// let _ = leaked;
    /// ```
    pub fn root_mut<'borrow, T: Persist>(&'borrow mut self, root: &Root<T>) -> PMut<'borrow, T> {
        PMut::from_id(root.id())
    }

    pub fn write_root<T: Persist>(&mut self, root: &Root<T>, value: T) {
        crate::marker::mark_transaction_func();

        let dst = unsafe { crate::marker::persistent_addr_mut(root.id()) }
            .cast::<core::mem::MaybeUninit<u8>>();
        let src = core::ptr::addr_of!(value).cast::<core::mem::MaybeUninit<u8>>();
        for offset in 0..core::mem::size_of::<T>() {
            unsafe {
                let byte = core::ptr::read_volatile(src.add(offset));
                core::ptr::write_volatile(dst.add(offset), byte);
            }
        }
        core::mem::forget(value);
    }
}
