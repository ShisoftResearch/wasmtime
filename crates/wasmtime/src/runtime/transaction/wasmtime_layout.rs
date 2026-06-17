use crate::prelude::*;
use crate::runtime::store::InstanceId;
use alloc::vec::Vec;

#[cfg(test)]
use super::ObjectValue;
use super::type_layout::{
    PersistentTypeKind, PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId,
};

pub(crate) const PERSISTENT_OBJECT_ABI_SLOT_SIZE: u32 = 20;
const WASMTIME_LAYOUT_FINGERPRINT_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const WASMTIME_LAYOUT_FINGERPRINT_PRIME: u64 = 0x0000_0001_0000_01b3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WasmtimePersistentFieldLayout {
    pub(crate) field_index: u32,
    pub(crate) field_offset: u32,
    pub(crate) value_size: u32,
    pub(crate) is_object_ref: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WasmtimePersistentFieldLayoutAbi {
    pub(crate) field_index: u32,
    pub(crate) field_offset: u32,
    pub(crate) value_size: u32,
    pub(crate) is_object_ref: u32,
}

impl WasmtimePersistentFieldLayoutAbi {
    pub(crate) fn to_field_layout(self) -> Result<WasmtimePersistentFieldLayout> {
        ensure!(
            self.is_object_ref <= 1,
            "transactional persistent field layout ref flag must be 0 or 1"
        );
        Ok(WasmtimePersistentFieldLayout {
            field_index: self.field_index,
            field_offset: self.field_offset,
            value_size: self.value_size,
            is_object_ref: self.is_object_ref != 0,
        })
    }
}

pub(crate) fn persistent_layout_for_wasmtime_struct_type(
    type_layout_id: TypeLayoutId,
    body_size: u32,
    fields: &[WasmtimePersistentFieldLayout],
) -> Result<PersistentTypeLayout> {
    let trace_fields = fields
        .iter()
        .map(|field| StructTraceField {
            field_index: field.field_index,
            field_offset: field.field_offset,
            value_size: field.value_size,
            kind: wasmtime_trace_slot_kind(field.is_object_ref),
        })
        .collect::<Vec<_>>();
    Ok(PersistentTypeLayout::Struct {
        id: type_layout_id,
        fingerprint: wasmtime_struct_layout_fingerprint(body_size, fields)?,
        body_size,
        fields: trace_fields,
    })
}

pub(crate) fn persistent_layout_for_wasmtime_array_type(
    type_layout_id: TypeLayoutId,
    element_size: u32,
    element_is_object_ref: bool,
) -> PersistentTypeLayout {
    PersistentTypeLayout::Array {
        id: type_layout_id,
        fingerprint: wasmtime_array_layout_fingerprint(element_size, element_is_object_ref),
        element_size,
        element_kind: wasmtime_trace_slot_kind(element_is_object_ref),
    }
}

pub(super) fn wasmtime_type_layout_namespace(owner_instance: Option<InstanceId>) -> Result<u32> {
    match owner_instance {
        Some(instance) => instance
            .as_u32()
            .checked_add(1)
            .context("transactional instance namespace overflow"),
        None => Ok(0),
    }
}

fn wasmtime_trace_slot_kind(is_object_ref: bool) -> TraceSlotKind {
    if is_object_ref {
        TraceSlotKind::ObjectRef
    } else {
        TraceSlotKind::Scalar
    }
}

pub(super) fn wasmtime_struct_layout_fingerprint(
    body_size: u32,
    fields: &[WasmtimePersistentFieldLayout],
) -> Result<u64> {
    let mut fingerprint = wasmtime_layout_fingerprint_seed(PersistentTypeKind::Struct);
    fingerprint = stable_fingerprint_u32(fingerprint, body_size);
    fingerprint = stable_fingerprint_u32(
        fingerprint,
        u32::try_from(fields.len()).context("wasmtime struct field count exceeds u32")?,
    );
    for field in fields {
        fingerprint = stable_fingerprint_u32(fingerprint, field.field_index);
        fingerprint = stable_fingerprint_u32(fingerprint, field.field_offset);
        fingerprint = stable_fingerprint_u32(fingerprint, field.value_size);
        fingerprint = stable_fingerprint_u8(fingerprint, u8::from(field.is_object_ref));
    }
    Ok(fingerprint)
}

pub(super) fn wasmtime_array_layout_fingerprint(
    element_size: u32,
    element_is_object_ref: bool,
) -> u64 {
    let mut fingerprint = wasmtime_layout_fingerprint_seed(PersistentTypeKind::Array);
    fingerprint = stable_fingerprint_u32(fingerprint, element_size);
    stable_fingerprint_u8(fingerprint, u8::from(element_is_object_ref))
}

fn wasmtime_layout_fingerprint_seed(kind: PersistentTypeKind) -> u64 {
    let fingerprint = WASMTIME_LAYOUT_FINGERPRINT_OFFSET_BASIS;
    stable_fingerprint_u16(fingerprint, kind as u16)
}

fn stable_fingerprint_u8(fingerprint: u64, value: u8) -> u64 {
    stable_fingerprint_bytes(fingerprint, &[value])
}

fn stable_fingerprint_u16(fingerprint: u64, value: u16) -> u64 {
    stable_fingerprint_bytes(fingerprint, &value.to_le_bytes())
}

fn stable_fingerprint_u32(fingerprint: u64, value: u32) -> u64 {
    stable_fingerprint_bytes(fingerprint, &value.to_le_bytes())
}

fn stable_fingerprint_bytes(mut fingerprint: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        fingerprint ^= u64::from(*byte);
        fingerprint = fingerprint.wrapping_mul(WASMTIME_LAYOUT_FINGERPRINT_PRIME);
    }
    fingerprint
}

fn wasmtime_struct_body_size_from_field_count(field_count: usize) -> Result<u32> {
    u32::try_from(field_count)
        .context("transactional struct field count exceeds u32")?
        .checked_mul(PERSISTENT_OBJECT_ABI_SLOT_SIZE)
        .context("transactional struct body size overflow")
}

pub(super) fn validate_wasmtime_struct_field_layouts(
    fields: &[WasmtimePersistentFieldLayout],
) -> Result<u32> {
    for (index, field) in fields.iter().enumerate() {
        let expected_index =
            u32::try_from(index).context("transactional struct field index overflow")?;
        let expected_offset = expected_index
            .checked_mul(PERSISTENT_OBJECT_ABI_SLOT_SIZE)
            .context("transactional struct field offset overflow")?;
        ensure!(
            field.field_index == expected_index,
            "transactional persistent struct layout field index is not canonical"
        );
        ensure!(
            field.field_offset == expected_offset,
            "transactional persistent struct layout field offset is not canonical"
        );
        ensure!(
            field.value_size > 0,
            "transactional persistent struct layout field value size is zero"
        );
        if field.is_object_ref {
            ensure!(
                field.value_size == PERSISTENT_OBJECT_ABI_SLOT_SIZE,
                "transactional persistent struct object-ref layout field must use object value ABI size"
            );
        }
    }
    wasmtime_struct_body_size_from_field_count(fields.len())
}

pub(super) fn validate_wasmtime_array_element_layout(
    element_size: u32,
    element_is_object_ref: bool,
) -> Result<u32> {
    ensure!(
        element_size > 0,
        "transactional persistent array element size is zero"
    );
    if element_is_object_ref {
        ensure!(
            element_size == PERSISTENT_OBJECT_ABI_SLOT_SIZE,
            "transactional persistent object-ref array element layout must use object value ABI size"
        );
    }
    Ok(element_size)
}

#[cfg(test)]
pub(super) fn wasmtime_struct_field_layouts_from_values(
    fields: &[ObjectValue],
) -> Result<(u32, Vec<WasmtimePersistentFieldLayout>)> {
    let mut layouts = Vec::with_capacity(fields.len());
    for (index, value) in fields.iter().enumerate() {
        let field_index =
            u32::try_from(index).context("transactional struct field index overflow")?;
        let field_offset = field_index
            .checked_mul(PERSISTENT_OBJECT_ABI_SLOT_SIZE)
            .context("transactional struct field offset overflow")?;
        layouts.push(WasmtimePersistentFieldLayout {
            field_index,
            field_offset,
            value_size: PERSISTENT_OBJECT_ABI_SLOT_SIZE,
            is_object_ref: matches!(value, ObjectValue::Ref(_)),
        });
    }
    let body_size = wasmtime_struct_body_size_from_field_count(fields.len())?;
    Ok((body_size, layouts))
}

#[cfg(test)]
pub(super) fn wasmtime_array_layout_from_values(
    type_layout_id: TypeLayoutId,
    elements: &[ObjectValue],
) -> PersistentTypeLayout {
    persistent_layout_for_wasmtime_array_type(
        type_layout_id,
        PERSISTENT_OBJECT_ABI_SLOT_SIZE,
        elements
            .iter()
            .any(|value| matches!(value, ObjectValue::Ref(_))),
    )
}

pub(super) fn wasmtime_array_layout_from_element_layout(
    type_layout_id: TypeLayoutId,
    element_size: u32,
    element_is_object_ref: bool,
) -> Result<PersistentTypeLayout> {
    Ok(persistent_layout_for_wasmtime_array_type(
        type_layout_id,
        validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?,
        element_is_object_ref,
    ))
}
