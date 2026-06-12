use crate::PersistentId;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "twasm_intrinsics")]
unsafe extern "C" {
    #[link_name = "__twasm_persistent_addr_mut"]
    fn wasm_persistent_addr_mut(raw_id: u64) -> *mut u8;

    #[link_name = "__twasm_mark_transaction_func"]
    fn wasm_mark_transaction_func();

    #[link_name = "__twasm_mark_persistent_arg"]
    fn wasm_mark_persistent_arg(index: u32);
}

pub unsafe fn persistent_addr_mut(id: PersistentId) -> *mut u8 {
    #[cfg(target_arch = "wasm32")]
    {
        unsafe { wasm_persistent_addr_mut(id.raw()) }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        id.payload() as usize as *mut u8
    }
}

pub fn mark_transaction_func() {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        wasm_mark_transaction_func();
    }
}

pub fn mark_persistent_arg(index: u32) {
    #[cfg(target_arch = "wasm32")]
    unsafe {
        wasm_mark_persistent_arg(index);
    }

    #[cfg(not(target_arch = "wasm32"))]
    let _ = index;
}
