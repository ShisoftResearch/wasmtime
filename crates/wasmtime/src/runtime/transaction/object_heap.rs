use super::{ObjectId, ObjectKind, ObjectPayload, ObjectRefValue, ObjectValue};
use crate::prelude::*;
#[cfg(test)]
use crate::runtime::vm::block_region::{BLOCK_SIZE, LINE_MARKED};
use crate::runtime::vm::block_region::{IMMIX_LINE_SIZE, VMemoryBlockRegion};
use alloc::vec::Vec;
use core::mem::size_of;

const DEFAULT_OBJECT_HEAP_BLOCKS: usize = 4;
const OBJECT_RECORD_ALIGN: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxRecordHandle(u64);

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxObjectHeader {
    pub(crate) record_len: u64,
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) flags: u16,
    pub(crate) type_index: u32,
}

impl TxObjectHeader {
    const BYTE_LEN: usize = size_of::<Self>();

    pub(crate) fn as_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        bytes[0..8].copy_from_slice(&self.record_len.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.object_id.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.version.to_le_bytes());
        bytes[20..22].copy_from_slice(&self.kind.to_le_bytes());
        bytes[22..24].copy_from_slice(&self.flags.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.type_index.to_le_bytes());
        bytes
    }

    pub(crate) fn read_from_prefix(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= Self::BYTE_LEN,
            "transaction object header prefix is shorter than expected"
        );
        Ok(Self {
            record_len: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            object_id: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            version: u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            kind: u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
            flags: u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
            type_index: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
        })
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxArrayHeader {
    pub(crate) base: TxObjectHeader,
    pub(crate) length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TraceValueKind {
    Scalar,
    ObjectIdRef,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TraceFieldDescriptor {
    pub(crate) field_index: u32,
    pub(crate) field_offset: u32,
    pub(crate) kind: TraceValueKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TraceArrayDescriptor {
    pub(crate) element_kind: TraceValueKind,
    pub(crate) length: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TraceDescriptor {
    Struct(Vec<TraceFieldDescriptor>),
    Array(TraceArrayDescriptor),
    Scalar,
}

#[derive(Clone, Debug, PartialEq)]
struct ObjectRecord {
    header: TxObjectHeader,
    array_length: Option<u32>,
    payload: ObjectPayload,
    offset: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ObjectHeap {
    records: Vec<ObjectRecord>,
    region: Option<ObjectHeapRegion>,
}

impl ObjectHeap {
    pub(crate) fn allocate_record(
        &mut self,
        object_id: ObjectId,
        version: u32,
        kind: ObjectKind,
        flags: u16,
        type_index: u32,
        payload: &ObjectPayload,
    ) -> Result<TxRecordHandle> {
        ensure!(
            kind == payload.kind(),
            "object payload kind does not match record kind"
        );

        let array_length = match payload {
            ObjectPayload::Array(elements) => Some(
                u32::try_from(elements.len()).context("array payload length does not fit u32")?,
            ),
            _ => None,
        };
        let record_len = logical_record_len(payload, array_length)?;
        let header = TxObjectHeader {
            record_len,
            object_id: object_id.object_index,
            version,
            kind: kind as u16,
            flags,
            type_index,
        };
        let bytes = serialize_record(&header, array_length, payload)?;
        let offset = self.region_mut()?.allocate(&bytes)?;
        self.records.push(ObjectRecord {
            header,
            array_length,
            payload: payload.clone(),
            offset,
        });
        let handle = u64::try_from(self.records.len()).context("record handle overflow")?;
        Ok(TxRecordHandle(handle))
    }

    pub(crate) fn header(&self, handle: TxRecordHandle) -> Result<TxObjectHeader> {
        Ok(self.record(handle)?.header)
    }

    pub(crate) fn array_header(&self, handle: TxRecordHandle) -> Result<TxArrayHeader> {
        let record = self.record(handle)?;
        let length = record
            .array_length
            .context("record does not contain an array header")?;
        Ok(TxArrayHeader {
            base: record.header,
            length,
        })
    }

    pub(crate) fn record_len(&self, handle: TxRecordHandle) -> Result<u64> {
        Ok(self.record(handle)?.header.record_len)
    }

    pub(crate) fn record_bytes(&self, handle: TxRecordHandle) -> Result<Vec<u8>> {
        let record = self.record(handle)?;
        self.region()?
            .read(record.offset, usize::try_from(record.header.record_len)?)
    }

    pub(crate) fn publication_record(
        &self,
        handle: TxRecordHandle,
    ) -> Result<(TxObjectHeader, Vec<u8>)> {
        let record = self.record(handle)?;
        let bytes = self
            .region()?
            .read(record.offset, usize::try_from(record.header.record_len)?)?;
        Ok((record.header, bytes))
    }

    pub(crate) fn payload(&self, handle: TxRecordHandle) -> Result<&ObjectPayload> {
        Ok(&self.record(handle)?.payload)
    }

    pub(crate) fn trace_descriptor(&self, handle: TxRecordHandle) -> Result<TraceDescriptor> {
        let record = self.record(handle)?;
        Ok(match &record.payload {
            ObjectPayload::Struct(fields) => TraceDescriptor::Struct(
                fields
                    .iter()
                    .enumerate()
                    .map(|(field_index, value)| {
                        Ok(TraceFieldDescriptor {
                            field_index: u32::try_from(field_index)
                                .context("field index does not fit u32")?,
                            field_offset: object_value_offset(fields, field_index)?,
                            kind: trace_value_kind(value),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            ObjectPayload::Array(elements) => TraceDescriptor::Array(TraceArrayDescriptor {
                element_kind: trace_array_element_kind(elements),
                length: record.array_length.unwrap_or(0),
            }),
            ObjectPayload::I31(_) | ObjectPayload::Extern(_) | ObjectPayload::Func(_) => {
                TraceDescriptor::Scalar
            }
        })
    }

    pub(crate) fn trace_object_ids(&self, handle: TxRecordHandle) -> Result<Vec<ObjectId>> {
        let record = self.record(handle)?;
        Ok(match &record.payload {
            ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => fields
                .iter()
                .filter_map(|value| match value {
                    ObjectValue::Ref(Some(object_id)) => Some(*object_id),
                    ObjectValue::Ref(None)
                    | ObjectValue::I32(_)
                    | ObjectValue::I64(_)
                    | ObjectValue::F32(_)
                    | ObjectValue::F64(_)
                    | ObjectValue::V128(_) => None,
                })
                .collect(),
            ObjectPayload::I31(_) | ObjectPayload::Extern(_) | ObjectPayload::Func(_) => Vec::new(),
        })
    }

    pub(crate) fn published_records(
        &self,
    ) -> impl Iterator<Item = (TxRecordHandle, TxObjectHeader)> + '_ {
        self.records.iter().enumerate().map(|(index, record)| {
            (
                TxRecordHandle(u64::try_from(index + 1).unwrap()),
                record.header,
            )
        })
    }

    fn record(&self, handle: TxRecordHandle) -> Result<&ObjectRecord> {
        let index = record_index(handle)?;
        self.records
            .get(index)
            .with_context(|| format!("object heap record is not live: {handle:?}"))
    }

    fn region(&self) -> Result<&ObjectHeapRegion> {
        self.region
            .as_ref()
            .context("object heap region has not been initialized")
    }

    fn region_mut(&mut self) -> Result<&mut ObjectHeapRegion> {
        if self.region.is_none() {
            self.region = Some(ObjectHeapRegion::new(DEFAULT_OBJECT_HEAP_BLOCKS)?);
        }
        Ok(self.region.as_mut().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn record_offset_for_test(&self, handle: TxRecordHandle) -> Result<usize> {
        Ok(self.record(handle)?.offset)
    }

    #[cfg(test)]
    pub(crate) fn record_bytes_for_test(&self, handle: TxRecordHandle) -> Result<Vec<u8>> {
        self.record_bytes(handle)
    }

    #[cfg(test)]
    pub(crate) fn block_region_block_size_for_test(&self) -> usize {
        BLOCK_SIZE
    }

    #[cfg(test)]
    pub(crate) fn immix_line_size_for_test(&self) -> usize {
        IMMIX_LINE_SIZE
    }

    #[cfg(test)]
    pub(crate) fn line_mark_for_record_for_test(&self, handle: TxRecordHandle) -> Result<bool> {
        let offset = self.record(handle)?.offset;
        self.region()?.line_mark_for_offset(offset)
    }
}

pub(crate) fn encode_object_record(
    object_id: u64,
    version: u32,
    kind: u16,
    type_index: u32,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let record_len = size_of::<TxObjectHeader>()
        .checked_add(payload.len())
        .context("object record length overflow")?;
    let header = TxObjectHeader {
        record_len: u64::try_from(record_len).context("object record length does not fit u64")?,
        object_id,
        version,
        kind,
        flags: 0,
        type_index,
    };

    let mut bytes = Vec::with_capacity(record_len);
    bytes.extend_from_slice(&header.as_bytes());
    bytes.extend_from_slice(payload);
    Ok(bytes)
}

#[cfg(test)]
pub(crate) fn encode_object_record_for_test(
    object_id: u64,
    version: u32,
    type_index: u32,
    payload: &[u8],
) -> Result<Vec<u8>> {
    encode_object_record(
        object_id,
        version,
        ObjectKind::Struct as u16,
        type_index,
        payload,
    )
}

#[derive(Debug)]
struct ObjectHeapRegion {
    backend: VMemoryBlockRegion,
    cursor: usize,
    limit: usize,
}

impl ObjectHeapRegion {
    fn new(block_count: usize) -> Result<Self> {
        let mut backend = VMemoryBlockRegion::new(block_count)?;
        let chunk = backend.alloc_chunk(block_count)?;
        backend.reset_line_marks(chunk)?;
        let range = chunk.byte_range();
        Ok(Self {
            backend,
            cursor: range.start,
            limit: range.end,
        })
    }

    fn allocate(&mut self, bytes: &[u8]) -> Result<usize> {
        ensure!(
            !bytes.is_empty(),
            "object heap cannot allocate an empty record"
        );
        let offset = align_up(self.cursor, OBJECT_RECORD_ALIGN)?;
        let end = offset
            .checked_add(bytes.len())
            .context("object heap allocation range overflow")?;
        ensure!(end <= self.limit, "object heap region is out of space");
        self.backend.write(offset, bytes)?;
        self.mark_allocated_lines(offset, bytes.len())?;
        self.cursor = end;
        Ok(offset)
    }

    fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .context("object heap read range overflow")?;
        ensure!(end <= self.cursor, "object heap read range out of bounds");
        self.backend.read(offset, len)
    }

    #[cfg(test)]
    fn line_mark_for_offset(&self, offset: usize) -> Result<bool> {
        Ok(self.backend.line_mark(offset / IMMIX_LINE_SIZE)? == LINE_MARKED)
    }

    fn mark_allocated_lines(&mut self, offset: usize, len: usize) -> Result<()> {
        let end = offset
            .checked_add(len)
            .context("object heap line mark range overflow")?;
        let last_byte = end
            .checked_sub(1)
            .context("object heap line mark range underflow")?;
        let first_line = offset / IMMIX_LINE_SIZE;
        let last_line = last_byte / IMMIX_LINE_SIZE;
        for line in first_line..=last_line {
            self.backend.mark_line(line)?;
        }
        Ok(())
    }
}

fn record_index(handle: TxRecordHandle) -> Result<usize> {
    let raw = handle.0.checked_sub(1).context("record handle underflow")?;
    usize::try_from(raw).context("record handle does not fit usize")
}

fn logical_record_len(payload: &ObjectPayload, array_length: Option<u32>) -> Result<u64> {
    let header_len = match array_length {
        Some(_) => size_of::<TxArrayHeader>(),
        None => size_of::<TxObjectHeader>(),
    };
    let payload_len = match payload {
        ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => fields
            .iter()
            .map(logical_object_value_len)
            .sum::<Result<u64>>()?,
        ObjectPayload::I31(_) => 4,
        ObjectPayload::Extern(_) => 8,
        ObjectPayload::Func(_) => 8,
    };
    let header_len = u64::try_from(header_len).context("record header length overflow")?;
    header_len
        .checked_add(payload_len)
        .context("record length overflow")
}

fn logical_object_value_len(value: &ObjectValue) -> Result<u64> {
    Ok(match value {
        ObjectValue::I32(_) | ObjectValue::F32(_) => 4,
        ObjectValue::I64(_) | ObjectValue::F64(_) => 8,
        ObjectValue::V128(_) => 16,
        ObjectValue::Ref(_) => 8,
    })
}

fn serialize_record(
    header: &TxObjectHeader,
    array_length: Option<u32>,
    payload: &ObjectPayload,
) -> Result<Vec<u8>> {
    let record_len =
        usize::try_from(header.record_len).context("record length does not fit usize")?;
    let mut bytes = Vec::with_capacity(record_len);
    bytes.extend_from_slice(&header.as_bytes());
    if let Some(length) = array_length {
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.resize(size_of::<TxArrayHeader>(), 0);
    } else {
        bytes.resize(size_of::<TxObjectHeader>(), 0);
    }
    append_payload_bytes(&mut bytes, payload)?;
    ensure!(
        bytes.len() == record_len,
        "serialized object record length mismatch"
    );
    Ok(bytes)
}

fn append_payload_bytes(bytes: &mut Vec<u8>, payload: &ObjectPayload) -> Result<()> {
    match payload {
        ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => {
            for field in fields {
                append_object_value_bytes(bytes, field)?;
            }
        }
        ObjectPayload::I31(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectPayload::Extern(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectPayload::Func(value) => bytes.extend_from_slice(&value.to_le_bytes()),
    }
    Ok(())
}

fn append_object_value_bytes(bytes: &mut Vec<u8>, value: &ObjectValue) -> Result<()> {
    match value {
        ObjectValue::I32(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectValue::I64(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectValue::F32(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectValue::F64(value) => bytes.extend_from_slice(&value.to_le_bytes()),
        ObjectValue::V128(value) => bytes.extend_from_slice(value),
        ObjectValue::Ref(object_id) => {
            bytes.extend_from_slice(
                &ObjectRefValue::from_optional_object_id(*object_id)?
                    .as_raw()
                    .to_le_bytes(),
            );
        }
    }
    Ok(())
}

fn trace_value_kind(value: &ObjectValue) -> TraceValueKind {
    match value {
        ObjectValue::Ref(_) => TraceValueKind::ObjectIdRef,
        ObjectValue::I32(_)
        | ObjectValue::I64(_)
        | ObjectValue::F32(_)
        | ObjectValue::F64(_)
        | ObjectValue::V128(_) => TraceValueKind::Scalar,
    }
}

fn trace_array_element_kind(elements: &[ObjectValue]) -> TraceValueKind {
    if elements
        .iter()
        .any(|element| matches!(element, ObjectValue::Ref(_)))
    {
        TraceValueKind::ObjectIdRef
    } else {
        TraceValueKind::Scalar
    }
}

fn align_up(value: usize, align: usize) -> Result<usize> {
    debug_assert!(align.is_power_of_two());
    let mask = align - 1;
    value
        .checked_add(mask)
        .map(|value| value & !mask)
        .context("object heap alignment overflow")
}

fn object_value_offset(values: &[ObjectValue], field_index: usize) -> Result<u32> {
    let offset = values
        .iter()
        .take(field_index)
        .map(logical_object_value_len)
        .sum::<Result<u64>>()?;
    u32::try_from(offset).context("field offset does not fit u32")
}
