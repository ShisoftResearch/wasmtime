use super::type_layout::{PersistentTypeLayout, TraceSlotKind};
use super::{ObjectId, ObjectKind, ObjectPayload, ObjectValue, ObjectValueAbi};
use crate::prelude::*;
use crate::runtime::vm::TxDataRecordHeader;
#[cfg(test)]
use crate::runtime::vm::block_region::LINE_MARKED;
use crate::runtime::vm::block_region::{
    BLOCK_SIZE, IMMIX_LINE_SIZE, RegionChunk, VMemoryBlockRegion,
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;
#[cfg(test)]
use core::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_OBJECT_HEAP_BLOCKS: usize = 4;
const OBJECT_RECORD_ALIGN: usize = 8;
const OBJECT_VALUE_RECORD_LEN: usize = 20;

#[cfg(test)]
static DECODE_RECORD_CALLS_FOR_TEST: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxRecordHandle(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectRecordLocation {
    pub(crate) chunk_start_block: u32,
    pub(crate) chunk_blocks: u32,
    pub(crate) data_block: u32,
    pub(crate) data_offset: u32,
    pub(crate) record_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TxObjectHeader {
    pub(crate) record_len: u64,
    pub(crate) object_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) flags: u16,
    pub(crate) type_layout_id: u32,
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
        bytes[24..28].copy_from_slice(&self.type_layout_id.to_le_bytes());
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
            type_layout_id: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
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

#[derive(Clone, Debug)]
enum ObjectRecordStorage {
    Volatile {
        offset: usize,
        location: ObjectRecordLocation,
        payload: ObjectPayload,
    },
    PersistentMapped {
        source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
        data_record_offset: usize,
        record_len: u64,
        location: ObjectRecordLocation,
    },
}

#[derive(Clone, Debug)]
struct ObjectRecord {
    header: TxObjectHeader,
    array_length: Option<u32>,
    storage: ObjectRecordStorage,
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
        type_layout_id: u32,
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
            type_layout_id,
        };
        let bytes = serialize_record(&header, array_length, payload)?;
        let location = self.region_mut()?.allocate(&bytes)?;
        self.records.push(ObjectRecord {
            header,
            array_length,
            storage: ObjectRecordStorage::Volatile {
                offset: location.absolute_offset()?,
                location,
                payload: payload.clone(),
            },
        });
        let handle = u64::try_from(self.records.len()).context("record handle overflow")?;
        Ok(TxRecordHandle(handle))
    }

    pub(crate) fn install_record_bytes(&mut self, bytes: &[u8]) -> Result<TxRecordHandle> {
        let (header, array_length, payload) = decode_record(bytes)?;
        let location = self.region_mut()?.allocate(bytes)?;
        self.records.push(ObjectRecord {
            header,
            array_length,
            storage: ObjectRecordStorage::Volatile {
                offset: location.absolute_offset()?,
                location,
                payload,
            },
        });
        let handle = u64::try_from(self.records.len()).context("record handle overflow")?;
        Ok(TxRecordHandle(handle))
    }

    pub(crate) fn install_mapped_persistent_record(
        &mut self,
        winner: &crate::runtime::vm::RecoveredObjectWinner,
        source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
        record_bytes: &[u8],
    ) -> Result<TxRecordHandle> {
        let (header, array_length) = decode_record_metadata(record_bytes)?;
        let location = ObjectRecordLocation {
            chunk_start_block: winner.data_block,
            chunk_blocks: 0,
            data_block: winner.data_block,
            data_offset: winner.data_offset,
            record_len: winner.record_len,
        };
        self.records.push(ObjectRecord {
            header,
            array_length,
            storage: ObjectRecordStorage::PersistentMapped {
                source,
                data_record_offset: winner.data_record_offset,
                record_len: winner.record_len,
                location,
            },
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

    pub(crate) fn record_location(&self, handle: TxRecordHandle) -> Result<ObjectRecordLocation> {
        Ok(self.record_location_from_record(self.record(handle)?))
    }

    pub(crate) fn record_bytes(&self, handle: TxRecordHandle) -> Result<Vec<u8>> {
        self.with_record_bytes(handle, |bytes| Ok(bytes.to_vec()))
    }

    pub(crate) fn publication_record(
        &self,
        handle: TxRecordHandle,
    ) -> Result<(TxObjectHeader, Vec<u8>)> {
        let header = self.record(handle)?.header;
        self.with_record_bytes(handle, |bytes| Ok((header, bytes.to_vec())))
    }

    pub(crate) fn payload(&self, handle: TxRecordHandle) -> Result<ObjectPayload> {
        let record = self.record(handle)?;
        match &record.storage {
            ObjectRecordStorage::Volatile { payload, .. } => Ok(payload.clone()),
            ObjectRecordStorage::PersistentMapped { .. } => self
                .with_record_bytes(handle, |bytes| {
                    decode_payload_bytes(record.header, record.array_length, bytes)
                }),
        }
    }

    pub(crate) fn payload_bytes(&self, handle: TxRecordHandle) -> Result<Vec<u8>> {
        self.with_payload_bytes(handle, |payload| Ok(payload.to_vec()))
    }

    pub(crate) fn trace_descriptor(&self, handle: TxRecordHandle) -> Result<TraceDescriptor> {
        let record = self.record(handle)?;
        let payload = self.payload(handle)?;
        Ok(match &payload {
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
        })
    }

    pub(crate) fn trace_object_ids(&self, handle: TxRecordHandle) -> Result<Vec<ObjectId>> {
        let record = self.record(handle)?;
        self.with_record_bytes(handle, |bytes| {
            Ok(
                match decode_payload_bytes(record.header, record.array_length, bytes)? {
                    ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => fields
                        .iter()
                        .filter_map(|value| match value {
                            ObjectValue::Ref(Some(object_id)) => Some(*object_id),
                            ObjectValue::Ref(None)
                            | ObjectValue::I31(_)
                            | ObjectValue::I32(_)
                            | ObjectValue::I64(_)
                            | ObjectValue::F32(_)
                            | ObjectValue::F64(_)
                            | ObjectValue::V128(_)
                            | ObjectValue::FuncRef(_)
                            | ObjectValue::ExternRef(_) => None,
                        })
                        .collect(),
                },
            )
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

    fn with_record_bytes<T>(
        &self,
        handle: TxRecordHandle,
        f: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let record = self.record(handle)?;
        match &record.storage {
            ObjectRecordStorage::Volatile { offset, .. } => {
                let bytes = self
                    .region()?
                    .read(*offset, usize::try_from(record.header.record_len)?)?;
                f(&bytes)
            }
            ObjectRecordStorage::PersistentMapped {
                source,
                data_record_offset,
                record_len,
                ..
            } => {
                let payload_offset = data_record_offset
                    .checked_add(size_of::<TxDataRecordHeader>())
                    .context("mapped persistent object payload offset overflow")?;
                let mut result = None;
                let mut f = Some(f);
                source.with_mapped_slice(
                    payload_offset,
                    usize::try_from(*record_len)
                        .context("mapped persistent object record length overflow")?,
                    &mut |bytes| {
                        let f = f.take().context("mapped record callback already used")?;
                        result = Some(f(bytes)?);
                        Ok(())
                    },
                )?;
                result.context("mapped persistent object record was not read")
            }
        }
    }

    pub(crate) fn with_payload_bytes<T>(
        &self,
        handle: TxRecordHandle,
        f: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        let array_length = self.record(handle)?.array_length;
        self.with_record_bytes(handle, |bytes| {
            let payload = bytes
                .get(payload_offset(array_length)..)
                .context("object heap payload is out of bounds")?;
            f(payload)
        })
    }

    fn record_location_from_record(&self, record: &ObjectRecord) -> ObjectRecordLocation {
        match &record.storage {
            ObjectRecordStorage::Volatile { location, .. }
            | ObjectRecordStorage::PersistentMapped { location, .. } => *location,
        }
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
        match &self.record(handle)?.storage {
            ObjectRecordStorage::Volatile { offset, .. } => Ok(*offset),
            ObjectRecordStorage::PersistentMapped { .. } => {
                bail!("persistent mapped object records do not have a volatile heap offset")
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn record_bytes_for_test(&self, handle: TxRecordHandle) -> Result<Vec<u8>> {
        self.record_bytes(handle)
    }

    #[cfg(test)]
    pub(crate) fn record_location_for_test(
        &self,
        handle: TxRecordHandle,
    ) -> Result<ObjectRecordLocation> {
        self.record_location(handle)
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
        let offset = self.record_offset_for_test(handle)?;
        self.region()?.line_mark_for_offset(offset)
    }

    #[cfg(test)]
    pub(crate) fn is_persistent_mapped_for_test(&self, handle: TxRecordHandle) -> Result<bool> {
        Ok(matches!(
            &self.record(handle)?.storage,
            ObjectRecordStorage::PersistentMapped { .. }
        ))
    }
}

#[cfg(test)]
pub(crate) fn reset_decode_record_calls_for_test() {
    DECODE_RECORD_CALLS_FOR_TEST.store(0, Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn decode_record_calls_for_test() -> usize {
    DECODE_RECORD_CALLS_FOR_TEST.load(Ordering::SeqCst)
}

pub(crate) fn encode_object_record(
    object_id: u64,
    version: u32,
    kind: u16,
    type_layout_id: u32,
    payload: &ObjectPayload,
) -> Result<Vec<u8>> {
    let array_length = match payload {
        ObjectPayload::Array(elements) => {
            Some(u32::try_from(elements.len()).context("array payload length does not fit u32")?)
        }
        _ => None,
    };
    let record_len = logical_record_len(payload, array_length)?;
    let header = TxObjectHeader {
        record_len,
        object_id,
        version,
        kind,
        flags: 0,
        type_layout_id,
    };

    serialize_record(&header, array_length, payload)
}

#[cfg(test)]
pub(crate) fn encode_object_record_for_test(
    object_id: u64,
    version: u32,
    type_layout_id: u32,
    payload: &ObjectPayload,
) -> Result<Vec<u8>> {
    encode_object_record(
        object_id,
        version,
        payload.kind() as u16,
        type_layout_id,
        payload,
    )
}

#[derive(Debug)]
struct ObjectHeapRegion {
    backend: VMemoryBlockRegion,
    current_chunk: RegionChunk,
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
            current_chunk: chunk,
            cursor: range.start,
            limit: range.end,
        })
    }

    fn allocate(&mut self, bytes: &[u8]) -> Result<ObjectRecordLocation> {
        ensure!(
            !bytes.is_empty(),
            "object heap cannot allocate an empty record"
        );
        if !self.current_chunk_can_fit(bytes.len())? {
            self.allocate_next_chunk(bytes.len())?;
        }
        let offset = align_up(self.cursor, OBJECT_RECORD_ALIGN)?;
        let end = offset
            .checked_add(bytes.len())
            .context("object heap allocation range overflow")?;
        ensure!(end <= self.limit, "object heap region is out of space");
        self.backend.write(offset, bytes)?;
        self.mark_allocated_lines(offset, bytes.len())?;
        self.cursor = end;
        let record_len = u64::try_from(bytes.len()).context("object record length overflow")?;
        let chunk_start_block = u32::try_from(self.current_chunk.start_block())
            .context("object heap chunk start block overflow")?;
        let chunk_blocks = u32::try_from(self.current_chunk.block_count())
            .context("object heap chunk block count overflow")?;
        let data_block =
            u32::try_from(offset / BLOCK_SIZE).context("object data block overflow")?;
        let data_offset =
            u32::try_from(offset % BLOCK_SIZE).context("object data block offset overflow")?;
        Ok(ObjectRecordLocation {
            chunk_start_block,
            chunk_blocks,
            data_block,
            data_offset,
            record_len,
        })
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

    fn current_chunk_can_fit(&self, bytes_len: usize) -> Result<bool> {
        let offset = align_up(self.cursor, OBJECT_RECORD_ALIGN)?;
        let end = offset
            .checked_add(bytes_len)
            .context("object heap allocation range overflow")?;
        Ok(end <= self.limit)
    }

    fn allocate_next_chunk(&mut self, bytes_len: usize) -> Result<()> {
        let required_blocks = required_object_chunk_blocks(bytes_len)?;
        let chunk_blocks = required_blocks.max(DEFAULT_OBJECT_HEAP_BLOCKS);
        let chunk = match self.backend.alloc_chunk(chunk_blocks) {
            Ok(chunk) => chunk,
            Err(_) => {
                let new_block_count = self
                    .backend
                    .num_blocks()
                    .checked_add(chunk_blocks)
                    .context("object heap block region growth overflow")?;
                self.backend
                    .grow_to_blocks(new_block_count)?
                    .context("object heap block region did not grow")?
            }
        };
        self.backend.reset_line_marks(chunk)?;
        let range = chunk.byte_range();
        self.current_chunk = chunk;
        self.cursor = range.start;
        self.limit = range.end;
        Ok(())
    }
}

impl ObjectRecordLocation {
    fn absolute_offset(self) -> Result<usize> {
        let data_block =
            usize::try_from(self.data_block).context("object data block index overflow")?;
        let data_offset =
            usize::try_from(self.data_offset).context("object data offset overflow")?;
        data_block
            .checked_mul(BLOCK_SIZE)
            .and_then(|offset| offset.checked_add(data_offset))
            .context("object record absolute offset overflow")
    }
}

fn record_index(handle: TxRecordHandle) -> Result<usize> {
    let raw = handle.0.checked_sub(1).context("record handle underflow")?;
    usize::try_from(raw).context("record handle does not fit usize")
}

pub(crate) fn payload_offset_for_recovery(array_length: Option<u32>) -> usize {
    payload_offset(array_length)
}

fn payload_offset(array_length: Option<u32>) -> usize {
    match array_length {
        Some(_) => size_of::<TxArrayHeader>(),
        None => size_of::<TxObjectHeader>(),
    }
}

fn logical_record_len(payload: &ObjectPayload, array_length: Option<u32>) -> Result<u64> {
    let header_len = payload_offset(array_length);
    let payload_len = match payload {
        ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => {
            u64::try_from(fields.len()).context("object payload field count overflow")?
                * u64::try_from(OBJECT_VALUE_RECORD_LEN)
                    .context("object value ABI size does not fit u64")?
        }
    };
    let header_len = u64::try_from(header_len).context("record header length overflow")?;
    header_len
        .checked_add(payload_len)
        .context("record length overflow")
}

fn decode_record(bytes: &[u8]) -> Result<(TxObjectHeader, Option<u32>, ObjectPayload)> {
    #[cfg(test)]
    DECODE_RECORD_CALLS_FOR_TEST.fetch_add(1, Ordering::SeqCst);

    let (header, array_length) = decode_record_metadata(bytes)?;
    let payload = decode_payload_bytes(header, array_length, bytes)?;
    Ok((header, array_length, payload))
}

fn decode_record_metadata(bytes: &[u8]) -> Result<(TxObjectHeader, Option<u32>)> {
    let header = TxObjectHeader::read_from_prefix(bytes)?;
    let record_len =
        usize::try_from(header.record_len).context("record length does not fit usize")?;
    ensure!(
        record_len == bytes.len(),
        "serialized object record length does not match header"
    );
    let array_length = if header.kind == ObjectKind::Array as u16 {
        ensure!(
            bytes.len() >= size_of::<TxArrayHeader>(),
            "serialized array record is shorter than expected"
        );
        Some(u32::from_le_bytes(
            bytes[TxObjectHeader::BYTE_LEN..TxObjectHeader::BYTE_LEN + size_of::<u32>()]
                .try_into()
                .unwrap(),
        ))
    } else {
        None
    };
    validate_payload_shape(header, array_length, bytes)?;
    Ok((header, array_length))
}

fn validate_payload_shape(
    header: TxObjectHeader,
    array_length: Option<u32>,
    bytes: &[u8],
) -> Result<()> {
    let payload_start = payload_offset(array_length);
    ensure!(
        bytes.len() >= payload_start,
        "serialized object record is shorter than expected"
    );
    let payload_bytes = &bytes[payload_start..];
    match header.kind {
        x if x == ObjectKind::Struct as u16 => {
            ensure!(
                payload_bytes.len() % OBJECT_VALUE_RECORD_LEN == 0,
                "serialized struct payload length is not a multiple of object ABI size"
            );
            Ok(())
        }
        x if x == ObjectKind::Array as u16 => {
            let length =
                usize::try_from(array_length.context("serialized array record is missing length")?)
                    .context("serialized array length does not fit usize")?;
            let expected_len = length
                .checked_mul(OBJECT_VALUE_RECORD_LEN)
                .context("serialized array payload length overflow")?;
            ensure!(
                payload_bytes.len() == expected_len,
                "serialized array payload length does not match array length"
            );
            Ok(())
        }
        x if x == ObjectKind::I31 as u16 => {
            bail!("i31 values are encoded inline, not as object-table records")
        }
        x if x == ObjectKind::Extern as u16 || x == ObjectKind::Func as u16 => {
            bail!("function and external references are durable values, not object-table records")
        }
        _ => bail!(
            "unknown object kind tag in persistent record header: {}",
            header.kind
        ),
    }
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
    }
    Ok(())
}

fn append_object_value_bytes(bytes: &mut Vec<u8>, value: &ObjectValue) -> Result<()> {
    let abi = ObjectValueAbi::from_object_value(value)?;
    let (tag, low, high) = abi.as_parts();
    bytes.extend_from_slice(&tag.to_le_bytes());
    bytes.extend_from_slice(&low.to_le_bytes());
    bytes.extend_from_slice(&high.to_le_bytes());
    Ok(())
}

fn decode_payload_bytes(
    header: TxObjectHeader,
    array_length: Option<u32>,
    bytes: &[u8],
) -> Result<ObjectPayload> {
    let payload_start = payload_offset(array_length);
    ensure!(
        bytes.len() >= payload_start,
        "serialized object record is shorter than expected"
    );
    let payload_bytes = &bytes[payload_start..];
    Ok(match header.kind {
        x if x == ObjectKind::Struct as u16 => {
            ensure!(
                payload_bytes.len() % OBJECT_VALUE_RECORD_LEN == 0,
                "serialized struct payload length is not a multiple of object ABI size"
            );
            ObjectPayload::Struct(decode_object_values(payload_bytes)?)
        }
        x if x == ObjectKind::Array as u16 => {
            let length =
                usize::try_from(array_length.context("serialized array record is missing length")?)
                    .context("serialized array length does not fit usize")?;
            let expected_len = length
                .checked_mul(OBJECT_VALUE_RECORD_LEN)
                .context("serialized array payload length overflow")?;
            ensure!(
                payload_bytes.len() == expected_len,
                "serialized array payload length does not match array length"
            );
            ObjectPayload::Array(decode_object_values(payload_bytes)?)
        }
        x if x == ObjectKind::I31 as u16 => {
            bail!("i31 values are encoded inline, not as object-table records")
        }
        x if x == ObjectKind::Extern as u16 || x == ObjectKind::Func as u16 => {
            bail!("function and external references are durable values, not object-table records")
        }
        _ => bail!(
            "unknown object kind tag in persistent record header: {}",
            header.kind
        ),
    })
}

pub(crate) fn decode_payload_bytes_for_recovery(
    header: TxObjectHeader,
    array_length: Option<u32>,
    bytes: &[u8],
) -> Result<ObjectPayload> {
    decode_payload_bytes(header, array_length, bytes)
}

fn decode_object_values(bytes: &[u8]) -> Result<Vec<ObjectValue>> {
    let abi_size = OBJECT_VALUE_RECORD_LEN;
    ensure!(
        bytes.len() % abi_size == 0,
        "serialized object value payload length is not a multiple of object ABI size"
    );
    bytes
        .chunks_exact(abi_size)
        .map(|chunk| {
            let tag = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let low = u64::from_le_bytes(chunk[4..12].try_into().unwrap());
            let high = u64::from_le_bytes(chunk[12..20].try_into().unwrap());
            ObjectValueAbi::from_parts(tag, low, high)?.to_object_value()
        })
        .collect()
}

pub(crate) fn trace_object_refs_with_layout(
    layout: &PersistentTypeLayout,
    payload: &[u8],
    out: &mut Vec<ObjectId>,
) -> Result<()> {
    match layout {
        PersistentTypeLayout::Struct { fields, .. } => {
            for field in fields {
                if field.kind != TraceSlotKind::ObjectRef {
                    continue;
                }
                ensure!(
                    field.value_size == OBJECT_VALUE_RECORD_LEN as u32,
                    "persistent struct object-ref field must use object value ABI size"
                );
                let field_offset = usize::try_from(field.field_offset)
                    .context("persistent struct trace field offset does not fit usize")?;
                let bytes = object_ref_slot_bytes(
                    payload,
                    field_offset,
                    "persistent struct trace field range exceeds payload length",
                )?;
                if let Some(object_id) = decode_object_ref_slot(bytes)? {
                    out.push(object_id);
                }
            }
            Ok(())
        }
        PersistentTypeLayout::Array {
            element_size,
            element_kind,
            ..
        } => {
            if *element_kind == TraceSlotKind::Scalar {
                return Ok(());
            }
            let element_size = usize::try_from(*element_size)
                .context("persistent object-ref array element size does not fit usize")?;
            ensure!(
                element_size == OBJECT_VALUE_RECORD_LEN,
                "persistent object-ref array elements must use object value ABI size"
            );
            ensure!(
                payload.len() % element_size == 0,
                "persistent object-ref array payload length is not divisible by element size"
            );
            for element_offset in (0..payload.len()).step_by(element_size) {
                let bytes = object_ref_slot_bytes(
                    payload,
                    element_offset,
                    "persistent object-ref array element range exceeds payload length",
                )?;
                if let Some(object_id) = decode_object_ref_slot(bytes)? {
                    out.push(object_id);
                }
            }
            Ok(())
        }
        PersistentTypeLayout::Scalar { .. } => Ok(()),
    }
}

fn object_ref_slot_bytes<'a>(
    payload: &'a [u8],
    offset: usize,
    err: &'static str,
) -> Result<&'a [u8]> {
    let end = offset
        .checked_add(OBJECT_VALUE_RECORD_LEN)
        .context("persistent object reference trace range overflow")?;
    ensure!(end <= payload.len(), "{}", err);
    Ok(&payload[offset..end])
}

fn decode_object_ref_slot(bytes: &[u8]) -> Result<Option<ObjectId>> {
    ensure!(
        bytes.len() == OBJECT_VALUE_RECORD_LEN,
        "persistent object reference trace slot length mismatch"
    );
    let tag = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let low = u64::from_le_bytes(bytes[4..12].try_into().unwrap());
    let high = u64::from_le_bytes(bytes[12..20].try_into().unwrap());
    match ObjectValueAbi::from_parts(tag, low, high)?.to_object_value()? {
        ObjectValue::Ref(object_id) => Ok(object_id),
        ObjectValue::I31(_) | ObjectValue::FuncRef(_) | ObjectValue::ExternRef(_) => Ok(None),
        _ => bail!(
            "persistent object reference slot is not encoded as a ref, i31, funcref, or externref ABI value"
        ),
    }
}

fn trace_value_kind(value: &ObjectValue) -> TraceValueKind {
    match value {
        ObjectValue::Ref(_) => TraceValueKind::ObjectIdRef,
        ObjectValue::I31(_)
        | ObjectValue::I32(_)
        | ObjectValue::I64(_)
        | ObjectValue::F32(_)
        | ObjectValue::F64(_)
        | ObjectValue::V128(_)
        | ObjectValue::FuncRef(_)
        | ObjectValue::ExternRef(_) => TraceValueKind::Scalar,
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

fn required_object_chunk_blocks(bytes_len: usize) -> Result<usize> {
    ensure!(
        bytes_len > 0,
        "object heap cannot allocate an empty record chunk"
    );
    bytes_len
        .checked_add(BLOCK_SIZE - 1)
        .map(|bytes| bytes / BLOCK_SIZE)
        .context("object heap chunk size overflow")
}

fn object_value_offset(values: &[ObjectValue], field_index: usize) -> Result<u32> {
    let offset = values
        .iter()
        .take(field_index)
        .map(logical_object_value_len)
        .sum::<Result<u64>>()?;
    u32::try_from(offset).context("field offset does not fit u32")
}

fn logical_object_value_len(_value: &ObjectValue) -> Result<u64> {
    u64::try_from(OBJECT_VALUE_RECORD_LEN).context("object value record length overflow")
}

#[cfg(test)]
mod tests {
    use super::{
        OBJECT_VALUE_RECORD_LEN, ObjectHeap, ObjectId, ObjectKind, ObjectPayload, ObjectValue,
        TxObjectHeader, encode_object_record, trace_object_refs_with_layout,
    };
    use crate::runtime::transaction::type_layout::{
        PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId,
    };
    use crate::runtime::transaction::{DurableExternIdentity, DurableFuncIdentity};
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn layout_test_struct(fields: Vec<StructTraceField>) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(99).unwrap(),
            fingerprint: 0x5354_5255_4354_0099,
            body_size: 0,
            fields,
        }
    }

    fn object_ref_slot(offset: u32) -> StructTraceField {
        StructTraceField {
            field_index: offset / u32::try_from(OBJECT_VALUE_RECORD_LEN).unwrap(),
            field_offset: offset,
            value_size: OBJECT_VALUE_RECORD_LEN as u32,
            kind: TraceSlotKind::ObjectRef,
        }
    }

    fn scalar_slot(offset: u32) -> StructTraceField {
        StructTraceField {
            field_index: offset / u32::try_from(OBJECT_VALUE_RECORD_LEN).unwrap(),
            field_offset: offset,
            value_size: 8,
            kind: TraceSlotKind::Scalar,
        }
    }

    fn encode_payload(values: &[ObjectValue]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            super::append_object_value_bytes(&mut bytes, value).unwrap();
        }
        bytes
    }

    fn durable_func_identity(layout_id: u32) -> DurableFuncIdentity {
        DurableFuncIdentity {
            module_fingerprint: 0x10_20_30_40_50_60_70_80,
            function_index: 7,
            type_layout_id: TypeLayoutId::new(layout_id).unwrap(),
        }
    }

    fn durable_extern_identity(layout_id: u32) -> DurableExternIdentity {
        DurableExternIdentity {
            namespace: 9,
            handle: 0xab_cd_ef,
            type_layout_id: TypeLayoutId::new(layout_id).unwrap(),
        }
    }

    #[test]
    fn object_record_header_uses_type_layout_id() {
        let record = encode_object_record(
            7,
            11,
            ObjectKind::Struct as u16,
            23,
            &ObjectPayload::Struct(vec![]),
        )
        .unwrap();
        let header = TxObjectHeader::read_from_prefix(record.as_slice()).unwrap();

        assert_eq!(header.type_layout_id, 23);
    }

    #[test]
    fn durable_func_value_round_trips_inside_struct_payload() {
        let value = ObjectValue::FuncRef(durable_func_identity(5));
        let record = encode_object_record(
            7,
            1,
            ObjectKind::Struct as u16,
            TypeLayoutId::DEFAULT_STRUCT.get(),
            &ObjectPayload::Struct(vec![value.clone()]),
        )
        .unwrap();

        let mut heap = ObjectHeap::default();
        let handle = heap.install_record_bytes(&record).unwrap();

        assert_eq!(
            heap.payload(handle).unwrap(),
            ObjectPayload::Struct(vec![value])
        );
    }

    #[test]
    fn durable_extern_value_round_trips_inside_struct_payload() {
        let value = ObjectValue::ExternRef(durable_extern_identity(6));
        let record = encode_object_record(
            8,
            1,
            ObjectKind::Struct as u16,
            TypeLayoutId::DEFAULT_STRUCT.get(),
            &ObjectPayload::Struct(vec![value.clone()]),
        )
        .unwrap();

        let mut heap = ObjectHeap::default();
        let handle = heap.install_record_bytes(&record).unwrap();

        assert_eq!(
            heap.payload(handle).unwrap(),
            ObjectPayload::Struct(vec![value])
        );
    }

    #[test]
    fn trace_object_refs_with_layout_struct_finds_object_refs() {
        let layout = layout_test_struct(vec![
            scalar_slot(0),
            object_ref_slot(OBJECT_VALUE_RECORD_LEN as u32),
            scalar_slot((OBJECT_VALUE_RECORD_LEN * 2) as u32),
            object_ref_slot((OBJECT_VALUE_RECORD_LEN * 3) as u32),
        ]);
        let payload = encode_payload(&[
            ObjectValue::I32(7),
            ObjectValue::Ref(Some(ObjectId { object_index: 11 })),
            ObjectValue::V128([0; 16]),
            ObjectValue::Ref(Some(ObjectId { object_index: 12 })),
        ]);

        let mut out = Vec::new();
        trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap();

        assert_eq!(
            out,
            vec![ObjectId { object_index: 11 }, ObjectId { object_index: 12 }]
        );
    }

    #[test]
    fn trace_object_refs_with_layout_struct_ignores_scalar_slots() {
        let layout = layout_test_struct(vec![
            scalar_slot(0),
            object_ref_slot(OBJECT_VALUE_RECORD_LEN as u32),
        ]);
        let payload = encode_payload(&[
            ObjectValue::Ref(Some(ObjectId { object_index: 1 })),
            ObjectValue::Ref(Some(ObjectId { object_index: 2 })),
        ]);

        let mut out = Vec::new();
        trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap();

        assert_eq!(out, vec![ObjectId { object_index: 2 }]);
    }

    #[test]
    fn trace_object_refs_with_layout_array_finds_object_refs() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(100).unwrap(),
            fingerprint: 0x4152_5241_5900_0100,
            element_size: OBJECT_VALUE_RECORD_LEN as u32,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let payload = encode_payload(&[
            ObjectValue::Ref(None),
            ObjectValue::Ref(Some(ObjectId { object_index: 21 })),
            ObjectValue::Ref(Some(ObjectId { object_index: 22 })),
        ]);

        let mut out = Vec::new();
        trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap();

        assert_eq!(
            out,
            vec![ObjectId { object_index: 21 }, ObjectId { object_index: 22 }]
        );
    }

    #[test]
    fn trace_object_refs_with_layout_ref_slot_ignores_inline_i31_leaf() {
        let layout = layout_test_struct(vec![
            object_ref_slot(0),
            object_ref_slot(OBJECT_VALUE_RECORD_LEN as u32),
        ]);
        let payload = encode_payload(&[
            ObjectValue::I31(7),
            ObjectValue::Ref(Some(ObjectId { object_index: 12 })),
        ]);

        let mut out = Vec::new();
        trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap();

        assert_eq!(out, vec![ObjectId { object_index: 12 }]);
    }

    #[test]
    fn trace_object_refs_with_layout_ref_slot_ignores_inline_func_and_extern_leaves() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(103).unwrap(),
            fingerprint: 0x4152_5241_5900_0103,
            element_size: OBJECT_VALUE_RECORD_LEN as u32,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let payload = encode_payload(&[
            ObjectValue::FuncRef(durable_func_identity(5)),
            ObjectValue::ExternRef(durable_extern_identity(6)),
            ObjectValue::Ref(Some(ObjectId { object_index: 12 })),
        ]);

        let mut out = Vec::new();
        trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap();

        assert_eq!(out, vec![ObjectId { object_index: 12 }]);
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_bad_struct_field_offset() {
        let layout = layout_test_struct(vec![object_ref_slot(1)]);
        let payload = encode_payload(&[ObjectValue::Ref(Some(ObjectId { object_index: 11 }))]);
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(
            err.to_string()
                .contains("persistent struct trace field range exceeds payload length")
        );
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_bad_array_payload_length() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(101).unwrap(),
            fingerprint: 0x4152_5241_5900_0101,
            element_size: OBJECT_VALUE_RECORD_LEN as u32,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let mut payload = encode_payload(&[ObjectValue::Ref(Some(ObjectId { object_index: 21 }))]);
        payload.push(0);
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(err.to_string().contains(
            "persistent object-ref array payload length is not divisible by element size"
        ));
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_non_abi_sized_struct_ref_field() {
        let layout = layout_test_struct(vec![StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: 8,
            kind: TraceSlotKind::ObjectRef,
        }]);
        let payload = encode_payload(&[ObjectValue::Ref(Some(ObjectId { object_index: 11 }))]);
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(
            err.to_string()
                .contains("persistent struct object-ref field must use object value ABI size")
        );
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_non_abi_sized_array_elements() {
        let layout = PersistentTypeLayout::Array {
            id: TypeLayoutId::new(102).unwrap(),
            fingerprint: 0x4152_5241_5900_0102,
            element_size: 8,
            element_kind: TraceSlotKind::ObjectRef,
        };
        let payload = encode_payload(&[ObjectValue::Ref(Some(ObjectId { object_index: 21 }))]);
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(
            err.to_string()
                .contains("persistent object-ref array elements must use object value ABI size")
        );
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_non_ref_abi_tag() {
        let layout = layout_test_struct(vec![StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: OBJECT_VALUE_RECORD_LEN as u32,
            kind: TraceSlotKind::ObjectRef,
        }]);
        let payload = encode_payload(&[ObjectValue::I32(7)]);
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(
            err.to_string().contains(
                "persistent object reference slot is not encoded as a ref, i31, funcref, or externref ABI value"
            )
        );
    }

    #[test]
    fn trace_object_refs_with_layout_rejects_noncanonical_ref_abi_payload() {
        let layout = layout_test_struct(vec![StructTraceField {
            field_index: 0,
            field_offset: 0,
            value_size: OBJECT_VALUE_RECORD_LEN as u32,
            kind: TraceSlotKind::ObjectRef,
        }]);
        let mut payload = encode_payload(&[ObjectValue::Ref(Some(ObjectId { object_index: 11 }))]);
        payload[12..20].copy_from_slice(&1u64.to_le_bytes());
        let mut out = Vec::new();

        let err = trace_object_refs_with_layout(&layout, &payload, &mut out).unwrap_err();

        assert!(
            err.to_string()
                .contains("non-canonical ref object value ABI payload")
        );
    }
}
