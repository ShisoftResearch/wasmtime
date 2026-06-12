#![cfg_attr(target_arch = "wasm32", no_std)]

#[allow(unused_extern_crates)]
extern crate self as wasmtime_transaction_sdk;

#[cfg(feature = "macros")]
pub use wasmtime_transaction_sdk_macros::Persist;

pub mod id {
    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
    pub enum PersistentIdKind {
        Root,
        Value,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
    pub struct PersistentId {
        raw: u64,
        kind: PersistentIdKind,
    }

    impl PersistentId {
        pub const fn new(raw: u64, kind: PersistentIdKind) -> Self {
            Self { raw, kind }
        }

        pub const fn raw(self) -> u64 {
            self.raw
        }

        pub const fn kind(self) -> PersistentIdKind {
            self.kind
        }
    }
}

pub mod marker {
    pub fn mark_transaction_func() {}
}

pub mod persist {
    pub trait Persist {}
}

pub mod root {
    use core::marker::PhantomData;

    #[derive(Debug)]
    pub struct Root<T: ?Sized> {
        _marker: PhantomData<T>,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct PRef<'a, T: ?Sized> {
        inner: &'a T,
    }

    #[derive(Debug)]
    pub struct PMut<'a, T: ?Sized> {
        inner: &'a mut T,
    }

    impl<T: ?Sized> Root<T> {
        pub const fn new() -> Self {
            Self {
                _marker: PhantomData,
            }
        }
    }

    impl<'a, T: ?Sized> PRef<'a, T> {
        pub const fn new(inner: &'a T) -> Self {
            Self { inner }
        }

        pub const fn get(self) -> &'a T {
            self.inner
        }
    }

    impl<'a, T: ?Sized> PMut<'a, T> {
        pub fn new(inner: &'a mut T) -> Self {
            Self { inner }
        }

        pub fn get(&mut self) -> &mut T {
            self.inner
        }
    }
}

pub mod tx {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct Tx {
        _private: (),
    }

    impl Tx {
        /// Constructs a placeholder transaction handle for the skeleton SDK.
        pub unsafe fn from_marker() -> Self {
            Self { _private: () }
        }
    }
}

pub use id::{PersistentId, PersistentIdKind};
pub use persist::Persist;
pub use root::{PMut, PRef, Root};
pub use tx::Tx;

#[macro_export]
macro_rules! transaction {
    ($tx:ident, $body:block) => {{
        $crate::marker::mark_transaction_func();
        let $tx = unsafe { $crate::Tx::from_marker() };
        $body
    }};
}

#[cfg(test)]
mod tests {
    use super::persist::Persist as PersistTrait;

    #[derive(wasmtime_transaction_sdk_macros::Persist)]
    struct DerivedPersist;

    #[wasmtime_transaction_sdk_macros::transaction]
    fn passthrough_attribute() -> &'static str {
        "ok"
    }

    fn assert_persist<T: PersistTrait>() {}

    #[test]
    fn derive_marks_type_as_persist() {
        assert_persist::<DerivedPersist>();
    }

    #[test]
    fn bang_macro_executes_body() {
        let saw_tx = transaction!(tx, {
            let _ = tx;
            true
        });

        assert!(saw_tx);
    }

    #[test]
    fn attribute_macro_is_passthrough() {
        assert_eq!(passthrough_attribute(), "ok");
    }
}
