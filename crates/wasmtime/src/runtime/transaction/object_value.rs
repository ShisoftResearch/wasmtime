use crate::prelude::*;
use alloc::vec::Vec;

use super::ObjectId;
use super::durable_ref::{DurableExternIdentity, DurableFuncIdentity};
use super::type_layout::TypeLayoutId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct I31Value(u32);

impl I31Value {
    pub(super) const MASK: u32 = 0x7fff_ffff;

    pub(super) fn new(value: i32) -> Self {
        Self((value as u32) & Self::MASK)
    }

    pub(crate) fn get_s(self) -> i32 {
        ((self.0 << 1) as i32) >> 1
    }

    pub(crate) fn get_u(self) -> i32 {
        i32::try_from(self.0).unwrap()
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectKind {
    Struct,
    Array,
    I31,
    Extern,
    Func,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObjectValue {
    I31(i32),
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
    Ref(Option<ObjectId>),
    FuncRef(DurableFuncIdentity),
    ExternRef(DurableExternIdentity),
}

/// Durable raw ABI for persistent heap-object references in object payloads and
/// recovered root records.
///
/// This is intentionally separate from the live Wasm reference helper ABI:
/// lowering/libcalls may still see process-local `VMGcRef` handles or inline
/// i31 immediates while executing a transaction. Persistent object records
/// only store `ObjectId` identity here: `0` is null and every non-zero value is
/// `ObjectId.object_index + 1`.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PersistentObjectRefRaw(u64);

impl PersistentObjectRefRaw {
    pub(crate) fn from_optional_object_id(object_id: Option<ObjectId>) -> Result<Self> {
        Self::from_optional_object_index(object_id.map(|object_id| object_id.object_index))
    }

    pub(crate) fn from_optional_object_index(object_index: Option<u64>) -> Result<Self> {
        Ok(match object_index {
            Some(object_index) => Self(
                object_index
                    .checked_add(1)
                    .context("object reference encoding overflow")?,
            ),
            None => Self(0),
        })
    }

    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }

    pub(crate) fn decode(self) -> Option<ObjectId> {
        self.0
            .checked_sub(1)
            .map(|object_index| ObjectId { object_index })
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TransactionObjectRefRaw(u32);

pub(super) const FIRST_TRANSACTION_OBJECT_REF_HANDLE: u32 = 0x8000_0000;
pub(super) const TRANSACTION_OBJECT_REF_HANDLE_STEP: u32 = 2;

impl TransactionObjectRefRaw {
    pub(crate) fn from_optional_handle(handle: Option<u32>) -> Result<Self> {
        Ok(match handle {
            Some(0) => bail!("transaction object ref handle cannot be zero"),
            Some(handle) => Self(handle),
            None => Self(0),
        })
    }

    pub(crate) fn from_handle(handle: u32) -> Result<Self> {
        Self::from_optional_handle(Some(handle))
    }

    pub(crate) fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u32 {
        self.0
    }

    pub(crate) fn decode(self) -> Option<u32> {
        (self.0 != 0).then_some(self.0)
    }
}

pub(crate) const OBJECT_VALUE_ABI_TAG_I32: u32 = 0;
pub(crate) const OBJECT_VALUE_ABI_TAG_I64: u32 = 1;
pub(crate) const OBJECT_VALUE_ABI_TAG_F32: u32 = 2;
pub(crate) const OBJECT_VALUE_ABI_TAG_F64: u32 = 3;
pub(crate) const OBJECT_VALUE_ABI_TAG_V128: u32 = 4;
pub(crate) const OBJECT_VALUE_ABI_TAG_REF: u32 = 5;
pub(crate) const OBJECT_VALUE_ABI_TAG_FUNCREF: u32 = 6;
pub(crate) const OBJECT_VALUE_ABI_TAG_EXTERNREF: u32 = 7;
pub(crate) const OBJECT_VALUE_ABI_TAG_I31: u32 = 8;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED: u64 = 0;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_GC: u64 = 1;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC: u64 = 2;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_I31: u64 = 3;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN: u64 = 4;
pub(crate) const OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT: u64 = 5;
pub(super) const VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED: &str =
    "volatile GC reference promotion into persistent object graph is not implemented yet";

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectValueAbi {
    tag: u32,
    low: u64,
    high: u64,
}

impl ObjectValueAbi {
    pub(crate) fn from_parts(tag: u32, low: u64, high: u64) -> Result<Self> {
        match tag {
            OBJECT_VALUE_ABI_TAG_I32 => ensure!(
                high == 0 && low <= u64::from(u32::MAX),
                "non-canonical i32 object value ABI payload"
            ),
            OBJECT_VALUE_ABI_TAG_I31 => ensure!(
                high == 0 && low <= u64::from(I31Value::MASK),
                "non-canonical i31 object value ABI payload"
            ),
            OBJECT_VALUE_ABI_TAG_I64 => {
                ensure!(high == 0, "non-canonical i64 object value ABI payload")
            }
            OBJECT_VALUE_ABI_TAG_F32 => ensure!(
                high == 0 && low <= u64::from(u32::MAX),
                "non-canonical f32 object value ABI payload"
            ),
            OBJECT_VALUE_ABI_TAG_F64 => {
                ensure!(high == 0, "non-canonical f64 object value ABI payload")
            }
            OBJECT_VALUE_ABI_TAG_V128 => {}
            OBJECT_VALUE_ABI_TAG_REF => {
                ensure!(high == 0, "non-canonical ref object value ABI payload")
            }
            OBJECT_VALUE_ABI_TAG_FUNCREF | OBJECT_VALUE_ABI_TAG_EXTERNREF => {
                ensure!(
                    u32::try_from(high >> 32).unwrap() != 0,
                    "non-canonical durable ref object value ABI payload"
                );
            }
            _ => bail!("unknown object value ABI tag: {tag}"),
        }
        Ok(Self { tag, low, high })
    }

    pub(crate) fn from_live_parts(tag: u32, low: u64, high: u64) -> Result<Self> {
        if tag != OBJECT_VALUE_ABI_TAG_REF {
            return Self::from_parts(tag, low, high);
        }
        ensure!(
            matches!(
                high,
                OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED
                    | OBJECT_VALUE_ABI_LIVE_REF_KIND_GC
                    | OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC
                    | OBJECT_VALUE_ABI_LIVE_REF_KIND_I31
                    | OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN
                    | OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT
            ),
            "unknown live ref object value ABI kind"
        );
        Ok(Self { tag, low, high })
    }

    pub(crate) fn as_parts(self) -> (u32, u64, u64) {
        (self.tag, self.low, self.high)
    }

    pub(crate) fn from_object_value(value: &ObjectValue) -> Result<Self> {
        match value {
            ObjectValue::I31(value) => {
                let raw = u64::from((*value as u32) & I31Value::MASK);
                Self::from_parts(OBJECT_VALUE_ABI_TAG_I31, raw, 0)
            }
            ObjectValue::I32(value) => {
                Self::from_parts(OBJECT_VALUE_ABI_TAG_I32, u64::from(*value as u32), 0)
            }
            ObjectValue::I64(value) => Self::from_parts(OBJECT_VALUE_ABI_TAG_I64, *value as u64, 0),
            ObjectValue::F32(value) => {
                Self::from_parts(OBJECT_VALUE_ABI_TAG_F32, u64::from(*value), 0)
            }
            ObjectValue::F64(value) => Self::from_parts(OBJECT_VALUE_ABI_TAG_F64, *value, 0),
            ObjectValue::V128(value) => {
                let low = u64::from_le_bytes(value[0..8].try_into().unwrap());
                let high = u64::from_le_bytes(value[8..16].try_into().unwrap());
                Self::from_parts(OBJECT_VALUE_ABI_TAG_V128, low, high)
            }
            ObjectValue::Ref(object_id) => Self::from_parts(
                OBJECT_VALUE_ABI_TAG_REF,
                PersistentObjectRefRaw::from_optional_object_id(*object_id)?.as_raw(),
                0,
            ),
            ObjectValue::FuncRef(identity) => Self::from_parts(
                OBJECT_VALUE_ABI_TAG_FUNCREF,
                identity.module_fingerprint,
                u64::from(identity.function_index)
                    | (u64::from(identity.type_layout_id.get()) << 32),
            ),
            ObjectValue::ExternRef(identity) => Self::from_parts(
                OBJECT_VALUE_ABI_TAG_EXTERNREF,
                identity.handle,
                u64::from(identity.namespace) | (u64::from(identity.type_layout_id.get()) << 32),
            ),
        }
    }

    pub(crate) fn to_object_value(self) -> Result<ObjectValue> {
        let Self { tag, low, high } = Self::from_parts(self.tag, self.low, self.high)?;
        Ok(match tag {
            OBJECT_VALUE_ABI_TAG_I31 => ObjectValue::I31(I31Value::new(low as u32 as i32).get_s()),
            OBJECT_VALUE_ABI_TAG_I32 => ObjectValue::I32(low as u32 as i32),
            OBJECT_VALUE_ABI_TAG_I64 => ObjectValue::I64(low as i64),
            OBJECT_VALUE_ABI_TAG_F32 => ObjectValue::F32(low as u32),
            OBJECT_VALUE_ABI_TAG_F64 => ObjectValue::F64(low),
            OBJECT_VALUE_ABI_TAG_V128 => {
                let mut bytes = [0; 16];
                bytes[0..8].copy_from_slice(&low.to_le_bytes());
                bytes[8..16].copy_from_slice(&high.to_le_bytes());
                ObjectValue::V128(bytes)
            }
            OBJECT_VALUE_ABI_TAG_REF => {
                ObjectValue::Ref(PersistentObjectRefRaw::from_raw(low).decode())
            }
            OBJECT_VALUE_ABI_TAG_FUNCREF => ObjectValue::FuncRef(DurableFuncIdentity {
                module_fingerprint: low,
                function_index: u32::try_from(high & u64::from(u32::MAX)).unwrap(),
                type_layout_id: TypeLayoutId::new(u32::try_from(high >> 32).unwrap())
                    .context("durable func ref type layout id cannot be zero")?,
            }),
            OBJECT_VALUE_ABI_TAG_EXTERNREF => ObjectValue::ExternRef(DurableExternIdentity {
                namespace: u32::try_from(high & u64::from(u32::MAX)).unwrap(),
                handle: low,
                type_layout_id: TypeLayoutId::new(u32::try_from(high >> 32).unwrap())
                    .context("durable extern ref type layout id cannot be zero")?,
            }),
            _ => unreachable!(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObjectPayload {
    Struct(Vec<ObjectValue>),
    Array(Vec<ObjectValue>),
}

impl ObjectPayload {
    pub(super) fn kind(&self) -> ObjectKind {
        match self {
            ObjectPayload::Struct(_) => ObjectKind::Struct,
            ObjectPayload::Array(_) => ObjectKind::Array,
        }
    }

    pub(super) fn default_for_kind(kind: ObjectKind) -> Result<Self> {
        Ok(match kind {
            ObjectKind::Struct => Self::Struct(Vec::new()),
            ObjectKind::Array => Self::Array(Vec::new()),
            ObjectKind::I31 | ObjectKind::Extern | ObjectKind::Func => {
                bail!(
                    "i31, function, and external references are durable values, not object-table payloads"
                )
            }
        })
    }
}

pub(super) fn object_kind_from_u16(raw: u16) -> Result<ObjectKind> {
    match raw {
        x if x == ObjectKind::Struct as u16 => Ok(ObjectKind::Struct),
        x if x == ObjectKind::Array as u16 => Ok(ObjectKind::Array),
        x if x == ObjectKind::I31 as u16 => Ok(ObjectKind::I31),
        x if x == ObjectKind::Extern as u16 => Ok(ObjectKind::Extern),
        x if x == ObjectKind::Func as u16 => Ok(ObjectKind::Func),
        _ => bail!("unknown object kind tag in persistent record header: {raw}"),
    }
}
