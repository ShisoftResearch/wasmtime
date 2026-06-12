use core::marker::PhantomData;

#[derive(Debug, Default)]
pub struct Root<T: ?Sized> {
    _marker: PhantomData<fn() -> T>,
}

#[derive(Clone, Copy, Debug)]
pub struct PRef<'a, T: ?Sized> {
    _marker: PhantomData<&'a T>,
}

#[derive(Debug)]
pub struct PMut<'a, T: ?Sized> {
    _marker: PhantomData<&'a mut T>,
}
