#![cfg_attr(target_arch = "wasm32", no_std)]

#[allow(unused_extern_crates)]
extern crate self as wasmtime_transaction_sdk;

#[cfg(feature = "macros")]
pub use wasmtime_transaction_sdk_macros::{Persist, transaction as transaction_attr};

pub mod id;
pub mod marker;
pub mod persist;
pub mod root;
pub mod tx;

pub use id::{PersistentId, PersistentIdKind};
pub use persist::Persist;
pub use root::{PMut, PRef, Root};
pub use tx::Tx;

#[macro_export]
macro_rules! transaction {
    ($tx:ident, $body:block) => {{
        $crate::marker::mark_transaction_func();
        let mut $tx = unsafe { $crate::Tx::from_marker() };
        $body
    }};
}

#[cfg(test)]
mod tests {
    use crate::{Persist, Root};

    #[repr(C)]
    struct Counter {
        value: u32,
    }

    unsafe impl Persist for Counter {
        const TYPE_NAME: &'static str = "Counter";
    }

    #[test]
    fn transaction_macro_allows_root_mut() {
        let root = Root::<Counter>::tmemory_offset(0);

        crate::transaction!(tx, {
            let _ = tx.root_mut(&root);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    #[should_panic(
        expected = "persistent references are only materialized for wasm32 transaction guests"
    )]
    fn host_persistent_addr_materialization_panics() {
        let _ =
            unsafe { crate::marker::persistent_addr_mut(crate::PersistentId::tmemory_offset(0)) };
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    #[should_panic(
        expected = "persistent references are only materialized for wasm32 transaction guests"
    )]
    fn host_persistent_ref_deref_panics() {
        let root = Root::<Counter>::tmemory_offset(0);
        let tx = unsafe { crate::Tx::from_marker() };
        let _ = tx.root_ref(&root).as_ref();
    }
}
