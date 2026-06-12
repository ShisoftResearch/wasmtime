use core::marker::PhantomData;

use crate::{Persist, PersistentId, marker};

#[derive(Clone, Copy)]
pub struct Root<T: Persist> {
    id: PersistentId,
    _marker: PhantomData<T>,
}

impl<T: Persist> Root<T> {
    pub const fn from_id(id: PersistentId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub const fn tmemory_offset(offset: u32) -> Self {
        Self::from_id(PersistentId::tmemory_offset(offset))
    }

    pub const fn id(&self) -> PersistentId {
        self.id
    }
}

#[derive(Clone, Copy)]
pub struct PRef<'tx, T: Persist> {
    id: PersistentId,
    _marker: PhantomData<&'tx T>,
}

impl<'tx, T: Persist> PRef<'tx, T> {
    pub(crate) const fn from_id(id: PersistentId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub fn as_ref(self) -> &'tx T {
        unsafe { &*(marker::persistent_addr_mut(self.id) as *const T) }
    }
}

pub struct PMut<'tx, T: Persist> {
    id: PersistentId,
    _marker: PhantomData<&'tx mut T>,
}

impl<'tx, T: Persist> PMut<'tx, T> {
    pub(crate) const fn from_id(id: PersistentId) -> Self {
        Self {
            id,
            _marker: PhantomData,
        }
    }

    pub fn as_mut(self) -> &'tx mut T {
        unsafe { &mut *(marker::persistent_addr_mut(self.id) as *mut T) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Pair {
        a: u32,
        b: u32,
    }

    unsafe impl Persist for Pair {
        const TYPE_NAME: &'static str = "Pair";
    }

    #[test]
    fn root_keeps_typed_persistent_identity() {
        let root = Root::<Pair>::tmemory_offset(64);
        assert_eq!(root.id().payload(), 64);
    }
}
