#![allow(dead_code)]

use crate::prelude::*;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

const TYPE_LAYOUT_RECORD_MAGIC: u32 = 0x544c_4159; // "TLAY"
const MAX_TYPE_LAYOUT_RECORD_LEN: usize = 64 * 1024;
const MAX_STRUCT_TRACE_FIELDS: usize = 4096;
const TYPE_LAYOUT_RECORD_LEN_OFFSET: usize = 4;
const TYPE_LAYOUT_RECORD_KIND_OFFSET: usize = 12;
const TYPE_LAYOUT_RECORD_RESERVED_OFFSET: usize = 14;
const TYPE_LAYOUT_RECORD_BODY_SIZE_OR_ELEMENT_SIZE_OFFSET: usize = 24;
const TYPE_LAYOUT_RECORD_ELEMENT_KIND_OFFSET: usize = 32;
const TYPE_LAYOUT_RECORD_RESERVED2_OFFSET: usize = 33;
const TYPE_LAYOUT_FIELD_KIND_OFFSET: usize = TypeLayoutRecordHeader::BYTE_LEN + 12;
const TYPE_LAYOUT_FIELD_RESERVED_OFFSET: usize = TypeLayoutRecordHeader::BYTE_LEN + 13;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TypeLayoutId(u32);

impl TypeLayoutId {
    pub(crate) const DEFAULT_STRUCT: Self = Self(1);
    pub(crate) const DEFAULT_ARRAY: Self = Self(2);
    pub(crate) const BUILTIN_I31: Self = Self(3);
    pub(crate) const BUILTIN_EXTERN: Self = Self(4);
    pub(crate) const BUILTIN_FUNC: Self = Self(5);

    pub(crate) const fn new(raw: u32) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u16)]
pub(crate) enum PersistentTypeKind {
    Struct = 1,
    Array = 2,
    I31 = 3,
    Extern = 4,
    Func = 5,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum TraceSlotKind {
    Scalar = 0,
    ObjectRef = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StructTraceField {
    pub(crate) field_index: u32,
    pub(crate) field_offset: u32,
    pub(crate) value_size: u32,
    pub(crate) kind: TraceSlotKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PersistentTypeLayout {
    Struct {
        id: TypeLayoutId,
        fingerprint: u64,
        body_size: u32,
        fields: Vec<StructTraceField>,
    },
    Array {
        id: TypeLayoutId,
        fingerprint: u64,
        element_size: u32,
        element_kind: TraceSlotKind,
    },
    Scalar {
        id: TypeLayoutId,
        fingerprint: u64,
        kind: PersistentTypeKind,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TypeLayoutRegistry {
    layouts: BTreeMap<TypeLayoutId, PersistentTypeLayout>,
    fingerprints: BTreeMap<u64, Vec<TypeLayoutId>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TypeLayoutRecordHeader {
    magic: u32,
    record_len: u32,
    type_layout_id: u32,
    kind: u16,
    reserved: u16,
    fingerprint: u64,
    body_size_or_element_size: u32,
    field_count: u32,
    element_kind: u8,
    reserved2: [u8; 3],
}

impl TypeLayoutRecordHeader {
    const BYTE_LEN: usize = 36;

    fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..4].copy_from_slice(&self.magic.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.record_len.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.type_layout_id.to_le_bytes());
        bytes[12..14].copy_from_slice(&self.kind.to_le_bytes());
        bytes[14..16].copy_from_slice(&self.reserved.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.fingerprint.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.body_size_or_element_size.to_le_bytes());
        bytes[28..32].copy_from_slice(&self.field_count.to_le_bytes());
        bytes[32] = self.element_kind;
        bytes[33..36].copy_from_slice(&self.reserved2);
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "persistent type layout record header length mismatch"
        );
        let header = Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            record_len: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            type_layout_id: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            kind: u16::from_le_bytes(bytes[12..14].try_into().unwrap()),
            reserved: u16::from_le_bytes(bytes[14..16].try_into().unwrap()),
            fingerprint: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            body_size_or_element_size: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            field_count: u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            element_kind: bytes[32],
            reserved2: bytes[33..36].try_into().unwrap(),
        };
        ensure!(
            header.reserved == 0,
            "persistent type layout record reserved header field must be zero"
        );
        ensure!(
            header.reserved2 == [0; 3],
            "persistent type layout record reserved header padding must be zero"
        );
        Ok(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TypeLayoutFieldEntry {
    field_index: u32,
    field_offset: u32,
    value_size: u32,
    kind: TraceSlotKind,
}

impl TypeLayoutFieldEntry {
    const BYTE_LEN: usize = 16;

    fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..4].copy_from_slice(&self.field_index.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.field_offset.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.value_size.to_le_bytes());
        bytes[12] = self.kind as u8;
        bytes
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == Self::BYTE_LEN,
            "persistent type layout field entry length mismatch"
        );
        ensure!(
            bytes[13..16] == [0; 3],
            "persistent type layout field entry reserved padding must be zero"
        );
        Ok(Self {
            field_index: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            field_offset: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            value_size: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            kind: TraceSlotKind::from_raw(bytes[12])?,
        })
    }
}

impl PersistentTypeKind {
    fn from_raw(raw: u16) -> Result<Self> {
        match raw {
            raw if raw == Self::Struct as u16 => Ok(Self::Struct),
            raw if raw == Self::Array as u16 => Ok(Self::Array),
            raw if raw == Self::I31 as u16 => Ok(Self::I31),
            raw if raw == Self::Extern as u16 => Ok(Self::Extern),
            raw if raw == Self::Func as u16 => Ok(Self::Func),
            _ => bail!("unknown persistent type kind in record header: {raw}"),
        }
    }
}

impl TraceSlotKind {
    fn from_raw(raw: u8) -> Result<Self> {
        match raw {
            raw if raw == Self::Scalar as u8 => Ok(Self::Scalar),
            raw if raw == Self::ObjectRef as u8 => Ok(Self::ObjectRef),
            _ => bail!("unknown trace slot kind in record field entry: {raw}"),
        }
    }
}

impl PersistentTypeLayout {
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let (fingerprint, body_size_or_element_size, field_count, element_kind, record_len) =
            match self {
                Self::Struct {
                    fingerprint,
                    body_size,
                    fields,
                    ..
                } => {
                    let record_len = encoded_record_len_for_field_count(fields.len())?;
                    (
                        *fingerprint,
                        *body_size,
                        u32::try_from(fields.len())
                            .context("type layout field count exceeds u32")?,
                        TraceSlotKind::Scalar,
                        record_len,
                    )
                }
                Self::Array {
                    fingerprint,
                    element_size,
                    element_kind,
                    ..
                } => (
                    *fingerprint,
                    *element_size,
                    0,
                    *element_kind,
                    TypeLayoutRecordHeader::BYTE_LEN,
                ),
                Self::Scalar { fingerprint, .. } => (
                    *fingerprint,
                    0,
                    0,
                    TraceSlotKind::Scalar,
                    TypeLayoutRecordHeader::BYTE_LEN,
                ),
            };
        let header = TypeLayoutRecordHeader {
            magic: TYPE_LAYOUT_RECORD_MAGIC,
            record_len: u32::try_from(record_len)
                .context("type layout record length exceeds u32")?,
            type_layout_id: self.id().get(),
            kind: self.kind() as u16,
            reserved: 0,
            fingerprint,
            body_size_or_element_size,
            field_count,
            element_kind: element_kind as u8,
            reserved2: [0; 3],
        };
        let mut bytes = Vec::with_capacity(record_len);
        bytes.extend_from_slice(&header.as_bytes());
        if let Self::Struct { fields, .. } = self {
            for field in fields {
                let field = TypeLayoutFieldEntry {
                    field_index: field.field_index,
                    field_offset: field.field_offset,
                    value_size: field.value_size,
                    kind: field.kind,
                };
                bytes.extend_from_slice(&field.as_bytes());
            }
        }
        debug_assert_eq!(bytes.len(), record_len);
        Ok(bytes)
    }

    pub(crate) fn decode_prefix(bytes: &[u8]) -> Result<(Self, usize)> {
        ensure!(
            bytes.len() >= TypeLayoutRecordHeader::BYTE_LEN,
            "persistent type layout record is shorter than expected"
        );
        let header =
            TypeLayoutRecordHeader::from_bytes(&bytes[..TypeLayoutRecordHeader::BYTE_LEN])?;
        let (record_len, _) = validated_record_shape(header)?;
        ensure!(
            bytes.len() >= record_len,
            "persistent type layout buffer is shorter than one full record"
        );
        let layout = Self::decode_record(header, &bytes[..record_len])?;
        Ok((layout, record_len))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        let (layout, consumed) = Self::decode_prefix(bytes)?;
        ensure!(
            consumed == bytes.len(),
            "persistent type layout record length mismatch"
        );
        Ok(layout)
    }

    fn decode_record(header: TypeLayoutRecordHeader, bytes: &[u8]) -> Result<Self> {
        ensure!(
            header.magic == TYPE_LAYOUT_RECORD_MAGIC,
            "persistent type layout record has invalid magic"
        );
        let (_, field_count) = validated_record_shape(header)?;
        let id = TypeLayoutId::new(header.type_layout_id)
            .context("persistent type layout id cannot be zero")?;
        let kind = PersistentTypeKind::from_raw(header.kind)?;
        let element_kind = decode_header_element_kind(header.element_kind)?;
        match kind {
            PersistentTypeKind::Struct => {
                ensure!(
                    element_kind == TraceSlotKind::Scalar,
                    "persistent struct type layout header element kind must be scalar"
                );
                let mut fields = Vec::with_capacity(field_count);
                for index in 0..field_count {
                    let start = TypeLayoutRecordHeader::BYTE_LEN
                        + index
                            .checked_mul(TypeLayoutFieldEntry::BYTE_LEN)
                            .context("persistent type layout field offset overflow")?;
                    let end = start
                        .checked_add(TypeLayoutFieldEntry::BYTE_LEN)
                        .context("persistent type layout field end overflow")?;
                    let field = TypeLayoutFieldEntry::from_bytes(&bytes[start..end])?;
                    fields.push(StructTraceField {
                        field_index: field.field_index,
                        field_offset: field.field_offset,
                        value_size: field.value_size,
                        kind: field.kind,
                    });
                }
                Ok(Self::Struct {
                    id,
                    fingerprint: header.fingerprint,
                    body_size: header.body_size_or_element_size,
                    fields,
                })
            }
            PersistentTypeKind::Array => {
                ensure!(
                    field_count == 0,
                    "persistent array type layout must not contain field entries"
                );
                Ok(Self::Array {
                    id,
                    fingerprint: header.fingerprint,
                    element_size: header.body_size_or_element_size,
                    element_kind,
                })
            }
            PersistentTypeKind::I31 | PersistentTypeKind::Extern | PersistentTypeKind::Func => {
                ensure!(
                    field_count == 0,
                    "persistent scalar type layout must not contain field entries"
                );
                ensure!(
                    element_kind == TraceSlotKind::Scalar,
                    "persistent scalar type layout header element kind must be scalar"
                );
                ensure!(
                    header.body_size_or_element_size == 0,
                    "persistent scalar type layout header body size must be zero"
                );
                Ok(Self::Scalar {
                    id,
                    fingerprint: header.fingerprint,
                    kind,
                })
            }
        }
    }

    pub(crate) fn id(&self) -> TypeLayoutId {
        match self {
            Self::Struct { id, .. } | Self::Array { id, .. } | Self::Scalar { id, .. } => *id,
        }
    }

    pub(crate) fn kind(&self) -> PersistentTypeKind {
        match self {
            Self::Struct { .. } => PersistentTypeKind::Struct,
            Self::Array { .. } => PersistentTypeKind::Array,
            Self::Scalar { kind, .. } => *kind,
        }
    }

    pub(crate) fn fingerprint(&self) -> u64 {
        match self {
            Self::Struct { fingerprint, .. }
            | Self::Array { fingerprint, .. }
            | Self::Scalar { fingerprint, .. } => *fingerprint,
        }
    }

    fn shape_eq_ignoring_id(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Struct {
                    fingerprint: left_fingerprint,
                    body_size: left_body_size,
                    fields: left_fields,
                    ..
                },
                Self::Struct {
                    fingerprint: right_fingerprint,
                    body_size: right_body_size,
                    fields: right_fields,
                    ..
                },
            ) => {
                left_fingerprint == right_fingerprint
                    && left_body_size == right_body_size
                    && left_fields == right_fields
            }
            (
                Self::Array {
                    fingerprint: left_fingerprint,
                    element_size: left_element_size,
                    element_kind: left_element_kind,
                    ..
                },
                Self::Array {
                    fingerprint: right_fingerprint,
                    element_size: right_element_size,
                    element_kind: right_element_kind,
                    ..
                },
            ) => {
                left_fingerprint == right_fingerprint
                    && left_element_size == right_element_size
                    && left_element_kind == right_element_kind
            }
            (
                Self::Scalar {
                    fingerprint: left_fingerprint,
                    kind: left_kind,
                    ..
                },
                Self::Scalar {
                    fingerprint: right_fingerprint,
                    kind: right_kind,
                    ..
                },
            ) => left_fingerprint == right_fingerprint && left_kind == right_kind,
            _ => false,
        }
    }
}

impl TypeLayoutRegistry {
    pub(crate) fn insert(&mut self, layout: PersistentTypeLayout) -> Result<()> {
        let id = layout.id();
        ensure!(id.get() != 0, "persistent type layout id cannot be zero");

        if let Some(existing) = self.layouts.get(&id) {
            ensure!(
                existing == &layout,
                "conflicting persistent type layout for id {}",
                id.get()
            );
            return Ok(());
        }

        let fingerprint = layout.fingerprint();
        if let Some(existing_ids) = self.fingerprints.get_mut(&fingerprint) {
            for existing_id in existing_ids.iter().copied() {
                let existing = self.layouts.get(&existing_id).with_context(|| {
                    format!(
                        "persistent type layout fingerprint {fingerprint:#x} is registered to missing layout id {}",
                        existing_id.get()
                    )
                })?;
                ensure!(
                    existing.shape_eq_ignoring_id(&layout),
                    "persistent type layout fingerprint {fingerprint:#x} is already registered to incompatible layout id {}",
                    existing_id.get()
                );
            }
            existing_ids.push(id);
        } else {
            self.fingerprints.insert(fingerprint, vec![id]);
        }

        self.layouts.insert(id, layout);
        Ok(())
    }

    pub(crate) fn get(&self, id: TypeLayoutId) -> Option<&PersistentTypeLayout> {
        self.layouts.get(&id)
    }

    pub(crate) fn contains(&self, id: TypeLayoutId) -> bool {
        self.layouts.contains_key(&id)
    }

    pub(crate) fn ids_for_fingerprint(
        &self,
        fingerprint: u64,
    ) -> impl Iterator<Item = TypeLayoutId> + '_ {
        self.fingerprints
            .get(&fingerprint)
            .into_iter()
            .flat_map(|ids| ids.iter().copied())
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &PersistentTypeLayout> {
        self.layouts.values()
    }

    pub(crate) fn encode_all(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for layout in self.iter() {
            bytes.extend_from_slice(&layout.encode()?);
        }
        Ok(bytes)
    }

    pub(crate) fn decode_all(mut bytes: &[u8]) -> Result<Self> {
        let mut registry = Self::default();
        while !bytes.is_empty() {
            let (layout, consumed) = PersistentTypeLayout::decode_prefix(bytes)?;
            registry.insert(layout)?;
            bytes = &bytes[consumed..];
        }
        Ok(registry)
    }
}

fn decode_header_element_kind(raw: u8) -> Result<TraceSlotKind> {
    TraceSlotKind::from_raw(raw).context("invalid persistent type layout header element kind")
}

fn validated_record_shape(header: TypeLayoutRecordHeader) -> Result<(usize, usize)> {
    let record_len = usize::try_from(header.record_len)
        .context("persistent type layout record length does not fit usize")?;
    let field_count = usize::try_from(header.field_count)
        .context("persistent type layout field count does not fit usize")?;
    ensure!(
        field_count <= MAX_STRUCT_TRACE_FIELDS,
        "persistent type layout field count exceeds maximum"
    );
    ensure!(
        record_len <= MAX_TYPE_LAYOUT_RECORD_LEN,
        "persistent type layout record length exceeds maximum"
    );
    let expected_len = encoded_record_len_for_field_count(field_count)?;
    ensure!(
        record_len == expected_len,
        "persistent type layout record length does not match field count"
    );
    Ok((record_len, field_count))
}

fn encoded_record_len_for_field_count(field_count: usize) -> Result<usize> {
    ensure!(
        field_count <= MAX_STRUCT_TRACE_FIELDS,
        "persistent type layout field count exceeds maximum"
    );
    let field_bytes = field_count
        .checked_mul(TypeLayoutFieldEntry::BYTE_LEN)
        .context("persistent type layout field bytes overflow")?;
    let record_len = TypeLayoutRecordHeader::BYTE_LEN
        .checked_add(field_bytes)
        .context("persistent type layout record length overflow")?;
    ensure!(
        record_len <= MAX_TYPE_LAYOUT_RECORD_LEN,
        "persistent type layout record length exceeds maximum"
    );
    Ok(record_len)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_STRUCT_TRACE_FIELDS, PersistentTypeKind, PersistentTypeLayout, StructTraceField,
        TYPE_LAYOUT_FIELD_KIND_OFFSET, TYPE_LAYOUT_FIELD_RESERVED_OFFSET,
        TYPE_LAYOUT_RECORD_BODY_SIZE_OR_ELEMENT_SIZE_OFFSET,
        TYPE_LAYOUT_RECORD_ELEMENT_KIND_OFFSET, TYPE_LAYOUT_RECORD_KIND_OFFSET,
        TYPE_LAYOUT_RECORD_LEN_OFFSET, TYPE_LAYOUT_RECORD_MAGIC,
        TYPE_LAYOUT_RECORD_RESERVED_OFFSET, TYPE_LAYOUT_RECORD_RESERVED2_OFFSET, TraceSlotKind,
        TypeLayoutFieldEntry, TypeLayoutId, TypeLayoutRecordHeader, TypeLayoutRegistry,
        encoded_record_len_for_field_count,
    };
    use alloc::string::ToString;
    use alloc::vec::Vec;

    const TEST_MAX_TYPE_LAYOUT_RECORD_LEN: usize = 64 * 1024;
    const TEST_MAX_STRUCT_TRACE_FIELDS: usize = 4096;

    #[test]
    fn struct_layout_round_trips() {
        let layout = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(11).unwrap(),
            fingerprint: 0x1234_5678_9abc_def0,
            body_size: 40,
            fields: vec![
                StructTraceField {
                    field_index: 0,
                    field_offset: 0,
                    value_size: 4,
                    kind: TraceSlotKind::Scalar,
                },
                StructTraceField {
                    field_index: 1,
                    field_offset: 8,
                    value_size: 8,
                    kind: TraceSlotKind::Scalar,
                },
                StructTraceField {
                    field_index: 2,
                    field_offset: 16,
                    value_size: 8,
                    kind: TraceSlotKind::ObjectRef,
                },
            ],
        };

        let encoded = layout.encode().unwrap();
        let decoded = PersistentTypeLayout::decode(&encoded).unwrap();

        assert_eq!(decoded, layout);
    }

    #[test]
    fn array_layout_round_trips() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(7).unwrap(),
            fingerprint: 0x0102_0304_0506_0708,
            element_size: 8,
            element_kind: TraceSlotKind::ObjectRef,
        };

        let encoded = layout.encode().unwrap();
        let decoded = PersistentTypeLayout::decode(&encoded).unwrap();

        assert_eq!(decoded, layout);
    }

    #[test]
    fn scalar_layout_round_trips() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(3).unwrap(),
            fingerprint: 0xaabb_ccdd_eeff_0011,
            kind: PersistentTypeKind::Func,
        };

        let encoded = layout.encode().unwrap();
        let decoded = PersistentTypeLayout::decode(&encoded).unwrap();

        assert_eq!(decoded, layout);
    }

    #[test]
    fn type_layout_id_zero_is_rejected() {
        assert_eq!(TypeLayoutId::new(0), None);
    }

    #[test]
    fn unknown_layout_kind_is_rejected() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(5).unwrap(),
            fingerprint: 0x7788_9900_aabb_ccdd,
            kind: PersistentTypeKind::I31,
        };
        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_KIND_OFFSET..TYPE_LAYOUT_RECORD_KIND_OFFSET + 2]
            .copy_from_slice(&99u16.to_le_bytes());

        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();

        assert!(err.to_string().contains("unknown persistent type kind"));
    }

    #[test]
    fn invalid_record_length_is_rejected() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(9).unwrap(),
            fingerprint: 0x1111_2222_3333_4444,
            element_size: 16,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_LEN_OFFSET..TYPE_LAYOUT_RECORD_LEN_OFFSET + 4]
            .copy_from_slice(&8u32.to_le_bytes());

        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();

        assert!(err.to_string().contains("record length"));
    }

    #[test]
    fn too_short_buffer_is_rejected() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(4).unwrap(),
            fingerprint: 0x1234,
            kind: PersistentTypeKind::Extern,
        };
        let encoded = layout.encode().unwrap();
        let err = PersistentTypeLayout::decode(&encoded[..encoded.len() - 1]).unwrap_err();

        assert!(err.to_string().contains("shorter than expected"));
    }

    #[test]
    fn struct_field_with_invalid_trace_slot_kind_is_rejected() {
        let layout = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(13).unwrap(),
            fingerprint: 0x2233_4455_6677_8899,
            body_size: 24,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 8,
                kind: TraceSlotKind::ObjectRef,
            }],
        };
        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_FIELD_KIND_OFFSET] = 7;

        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();

        assert!(err.to_string().contains("unknown trace slot kind"));
    }

    #[test]
    fn nonzero_header_reserved_bytes_are_rejected() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(6).unwrap(),
            fingerprint: 0xfeed_face_cafe_beef,
            kind: PersistentTypeKind::I31,
        };

        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_RESERVED_OFFSET..TYPE_LAYOUT_RECORD_RESERVED_OFFSET + 2]
            .copy_from_slice(&1u16.to_le_bytes());
        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();
        assert!(err.to_string().contains("reserved"));

        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_RESERVED2_OFFSET..TYPE_LAYOUT_RECORD_RESERVED2_OFFSET + 3]
            .copy_from_slice(&[1, 0, 0]);
        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();
        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn nonzero_field_reserved_bytes_are_rejected() {
        let layout = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(8).unwrap(),
            fingerprint: 0x1000_2000_3000_4000,
            body_size: 16,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 8,
                kind: TraceSlotKind::ObjectRef,
            }],
        };
        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_FIELD_RESERVED_OFFSET..TYPE_LAYOUT_FIELD_RESERVED_OFFSET + 3]
            .copy_from_slice(&[0, 1, 0]);

        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();

        assert!(err.to_string().contains("reserved"));
    }

    #[test]
    fn struct_layout_rejects_non_scalar_element_kind() {
        let layout = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(10).unwrap(),
            fingerprint: 0x6677_8899_aabb_ccdd,
            body_size: 16,
            fields: vec![],
        };
        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_ELEMENT_KIND_OFFSET] = TraceSlotKind::ObjectRef as u8;

        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();

        assert!(err.to_string().contains("element kind"));
    }

    #[test]
    fn scalar_layout_rejects_nonzero_non_applicable_fields() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(12).unwrap(),
            fingerprint: 0x1111_3333_5555_7777,
            kind: PersistentTypeKind::Func,
        };

        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_ELEMENT_KIND_OFFSET] = TraceSlotKind::ObjectRef as u8;
        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();
        assert!(err.to_string().contains("element kind"));

        let mut encoded = layout.encode().unwrap();
        encoded[TYPE_LAYOUT_RECORD_BODY_SIZE_OR_ELEMENT_SIZE_OFFSET
            ..TYPE_LAYOUT_RECORD_BODY_SIZE_OR_ELEMENT_SIZE_OFFSET + 4]
            .copy_from_slice(&4u32.to_le_bytes());
        let err = PersistentTypeLayout::decode(&encoded).unwrap_err();
        assert!(err.to_string().contains("body size"));
    }

    #[test]
    fn decode_prefix_decodes_first_record_with_trailing_bytes() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(14).unwrap(),
            fingerprint: 0x8888_9999_aaaa_bbbb,
            element_size: 8,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let mut bytes = layout.encode().unwrap();
        bytes.extend_from_slice(&[9, 8, 7, 6]);

        let (decoded, consumed) = PersistentTypeLayout::decode_prefix(&bytes).unwrap();

        assert_eq!(decoded, layout);
        assert_eq!(consumed, layout.encode().unwrap().len());
    }

    #[test]
    fn decode_rejects_trailing_bytes_while_decode_prefix_accepts_them() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(15).unwrap(),
            fingerprint: 0x9999_aaaa_bbbb_cccc,
            kind: PersistentTypeKind::Extern,
        };
        let mut bytes = layout.encode().unwrap();
        bytes.extend_from_slice(&[1, 2, 3, 4]);

        let err = PersistentTypeLayout::decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("record length mismatch"));

        let (decoded, consumed) = PersistentTypeLayout::decode_prefix(&bytes).unwrap();
        assert_eq!(decoded, layout);
        assert_eq!(consumed, layout.encode().unwrap().len());
    }

    #[test]
    fn huge_field_count_is_rejected_before_allocation() {
        let field_count = TEST_MAX_STRUCT_TRACE_FIELDS + 1;
        let record_len =
            TypeLayoutRecordHeader::BYTE_LEN + field_count * TypeLayoutFieldEntry::BYTE_LEN;
        let mut bytes = vec![0; TypeLayoutRecordHeader::BYTE_LEN];
        bytes[0..4].copy_from_slice(&TYPE_LAYOUT_RECORD_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(&u32::try_from(record_len).unwrap().to_le_bytes());
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&(PersistentTypeKind::Struct as u16).to_le_bytes());
        bytes[24..28].copy_from_slice(&16u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&u32::try_from(field_count).unwrap().to_le_bytes());
        bytes[32] = TraceSlotKind::Scalar as u8;

        let err = PersistentTypeLayout::decode_prefix(&bytes).unwrap_err();

        assert!(err.to_string().contains("field count"));
    }

    #[test]
    fn record_length_above_max_is_rejected() {
        let mut bytes = vec![0; TypeLayoutRecordHeader::BYTE_LEN];
        bytes[0..4].copy_from_slice(&TYPE_LAYOUT_RECORD_MAGIC.to_le_bytes());
        bytes[4..8].copy_from_slice(
            &u32::try_from(TEST_MAX_TYPE_LAYOUT_RECORD_LEN + 1)
                .unwrap()
                .to_le_bytes(),
        );
        bytes[8..12].copy_from_slice(&1u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&(PersistentTypeKind::Func as u16).to_le_bytes());
        bytes[32] = TraceSlotKind::Scalar as u8;

        let err = PersistentTypeLayout::decode_prefix(&bytes).unwrap_err();

        assert!(err.to_string().contains("record length"));
    }

    #[test]
    fn encode_length_helper_rejects_implausible_field_count() {
        let err = encoded_record_len_for_field_count(MAX_STRUCT_TRACE_FIELDS + 1).unwrap_err();

        assert!(err.to_string().contains("field count"));
    }

    #[test]
    fn registry_accepts_idempotent_duplicate() {
        let layout = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(21).unwrap(),
            fingerprint: 0x2100_0000_0000_0001,
            kind: PersistentTypeKind::I31,
        };
        let mut registry = TypeLayoutRegistry::default();

        registry.insert(layout.clone()).unwrap();
        registry.insert(layout.clone()).unwrap();

        assert_eq!(registry.get(layout.id()), Some(&layout));
        assert_eq!(
            registry
                .ids_for_fingerprint(layout.fingerprint())
                .collect::<Vec<_>>(),
            vec![layout.id()]
        );
    }

    #[test]
    fn registry_rejects_conflicting_duplicate_id() {
        let id = TypeLayoutId::new(22).unwrap();
        let mut registry = TypeLayoutRegistry::default();
        registry
            .insert(PersistentTypeLayout::Scalar {
                id,
                fingerprint: 0x2200_0000_0000_0001,
                kind: PersistentTypeKind::Extern,
            })
            .unwrap();

        let err = registry
            .insert(PersistentTypeLayout::Scalar {
                id,
                fingerprint: 0x2200_0000_0000_0002,
                kind: PersistentTypeKind::Func,
            })
            .unwrap_err();

        assert!(err.to_string().contains("conflicting"));
    }

    #[test]
    fn registry_allows_same_shape_fingerprint_for_distinct_ids() {
        let fingerprint = 0x2300_0000_0000_0001;
        let mut registry = TypeLayoutRegistry::default();
        let first = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(23).unwrap(),
            fingerprint,
            body_size: 20,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 20,
                kind: TraceSlotKind::ObjectRef,
            }],
        };
        let second = PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(24).unwrap(),
            fingerprint,
            body_size: 20,
            fields: vec![StructTraceField {
                field_index: 0,
                field_offset: 0,
                value_size: 20,
                kind: TraceSlotKind::ObjectRef,
            }],
        };

        registry.insert(first.clone()).unwrap();
        registry.insert(second.clone()).unwrap();

        assert_eq!(registry.get(first.id()), Some(&first));
        assert_eq!(registry.get(second.id()), Some(&second));
        assert_eq!(
            registry
                .ids_for_fingerprint(fingerprint)
                .collect::<Vec<_>>(),
            vec![first.id(), second.id()]
        );
    }

    #[test]
    fn registry_ids_for_fingerprint_survives_encode_decode_round_trip() {
        let fingerprint = 0x2600_0000_0000_0001;
        let first = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(26).unwrap(),
            fingerprint,
            element_size: 20,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let second = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(27).unwrap(),
            fingerprint,
            element_size: 20,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let mut registry = TypeLayoutRegistry::default();
        registry.insert(first).unwrap();
        registry.insert(second).unwrap();

        let decoded = TypeLayoutRegistry::decode_all(&registry.encode_all().unwrap()).unwrap();

        assert_eq!(
            decoded.ids_for_fingerprint(fingerprint).collect::<Vec<_>>(),
            vec![
                TypeLayoutId::new(26).unwrap(),
                TypeLayoutId::new(27).unwrap()
            ]
        );
    }

    #[test]
    fn registry_rejects_conflicting_same_id_even_when_shape_differs() {
        let id = TypeLayoutId::new(25).unwrap();
        let mut registry = TypeLayoutRegistry::default();
        registry
            .insert(PersistentTypeLayout::Array {
                id,
                fingerprint: 0x2500_0000_0000_0001,
                element_size: 20,
                element_kind: TraceSlotKind::Scalar,
            })
            .unwrap();

        let err = registry
            .insert(PersistentTypeLayout::Array {
                id,
                fingerprint: 0x2500_0000_0000_0001,
                element_size: 20,
                element_kind: TraceSlotKind::ObjectRef,
            })
            .unwrap_err();

        assert!(err.to_string().contains("conflicting"));
    }

    #[test]
    fn registry_encode_all_decode_all_round_trips_multiple_layouts() {
        let layouts = vec![
            PersistentTypeLayout::Struct {
                id: TypeLayoutId::new(31).unwrap(),
                fingerprint: 0x3100_0000_0000_0001,
                body_size: 24,
                fields: vec![
                    StructTraceField {
                        field_index: 0,
                        field_offset: 0,
                        value_size: 8,
                        kind: TraceSlotKind::ObjectRef,
                    },
                    StructTraceField {
                        field_index: 1,
                        field_offset: 8,
                        value_size: 8,
                        kind: TraceSlotKind::Scalar,
                    },
                ],
            },
            PersistentTypeLayout::Array {
                id: TypeLayoutId::new(32).unwrap(),
                fingerprint: 0x3200_0000_0000_0001,
                element_size: 8,
                element_kind: TraceSlotKind::ObjectRef,
            },
            PersistentTypeLayout::Scalar {
                id: TypeLayoutId::new(33).unwrap(),
                fingerprint: 0x3300_0000_0000_0001,
                kind: PersistentTypeKind::Func,
            },
        ];
        let mut registry = TypeLayoutRegistry::default();
        for layout in &layouts {
            registry.insert(layout.clone()).unwrap();
        }

        let encoded = registry.encode_all().unwrap();
        let decoded = TypeLayoutRegistry::decode_all(&encoded).unwrap();

        assert_eq!(decoded.iter().cloned().collect::<Vec<_>>(), layouts);
    }

    #[test]
    fn registry_decode_all_rejects_invalid_concatenated_record_data() {
        let first = PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(41).unwrap(),
            fingerprint: 0x4100_0000_0000_0001,
            kind: PersistentTypeKind::I31,
        };
        let second = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(42).unwrap(),
            fingerprint: 0x4200_0000_0000_0001,
            element_size: 8,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let mut bytes = first.encode().unwrap();
        let second = second.encode().unwrap();
        bytes.extend_from_slice(&second[..second.len() - 1]);

        let err = TypeLayoutRegistry::decode_all(&bytes).unwrap_err();

        assert!(err.to_string().contains("shorter"));
    }
}
