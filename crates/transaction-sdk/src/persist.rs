pub unsafe trait Persist: Sized + 'static {
    const TYPE_NAME: &'static str;
    const SIZE: usize = core::mem::size_of::<Self>();
    const ALIGN: usize = core::mem::align_of::<Self>();
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
