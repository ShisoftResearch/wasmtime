use super::type_layout::TypeLayoutId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableFuncIdentity {
    pub(crate) module_fingerprint: u64,
    pub(crate) function_index: u32,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableExternIdentity {
    pub(crate) namespace: u32,
    pub(crate) handle: u64,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurableRefValue {
    Null,
    Object(super::ObjectId),
    I31(i32),
    Func(DurableFuncIdentity),
    Extern(DurableExternIdentity),
}
