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

    pub fn root_ref<T: Persist>(&self, root: &Root<T>) -> PRef<'tx, T> {
        PRef::from_id(root.id())
    }

    pub fn root_mut<T: Persist>(&mut self, root: &Root<T>) -> PMut<'tx, T> {
        PMut::from_id(root.id())
    }
}
