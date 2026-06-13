use crate::rust_alloc::{boxed::Box, string::String, vec::Vec};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistField {
    pub name: &'static str,
    pub type_name: &'static str,
    pub offset: usize,
    pub size: usize,
    pub align: usize,
}

pub unsafe trait Persist: Sized + 'static {
    const TYPE_NAME: &'static str;
    const SIZE: usize = core::mem::size_of::<Self>();
    const ALIGN: usize = core::mem::align_of::<Self>();
    const FIELDS: &'static [PersistField] = &[];
}

unsafe impl Persist for u8 {
    const TYPE_NAME: &'static str = "u8";
}

unsafe impl Persist for u16 {
    const TYPE_NAME: &'static str = "u16";
}

unsafe impl Persist for u32 {
    const TYPE_NAME: &'static str = "u32";
}

unsafe impl Persist for u64 {
    const TYPE_NAME: &'static str = "u64";
}

unsafe impl Persist for i32 {
    const TYPE_NAME: &'static str = "i32";
}

unsafe impl Persist for i64 {
    const TYPE_NAME: &'static str = "i64";
}

unsafe impl Persist for f32 {
    const TYPE_NAME: &'static str = "f32";
}

unsafe impl Persist for f64 {
    const TYPE_NAME: &'static str = "f64";
}

unsafe impl<T: Persist, const N: usize> Persist for [T; N] {
    const TYPE_NAME: &'static str = "[T; N]";
}

unsafe impl Persist for String {
    const TYPE_NAME: &'static str = "String";
}

unsafe impl<T: Persist> Persist for Vec<T> {
    const TYPE_NAME: &'static str = "Vec<T>";
}

unsafe impl<T: Persist> Persist for Box<T> {
    const TYPE_NAME: &'static str = "Box<T>";
}
