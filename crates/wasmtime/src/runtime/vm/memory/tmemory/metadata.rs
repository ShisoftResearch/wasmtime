#![allow(dead_code)]

use super::block_region::BLOCK_SIZE;
use crate::prelude::*;
use crate::runtime::transaction::type_layout::{PersistentTypeLayout, TypeLayoutRegistry};
use alloc::vec;
use alloc::vec::Vec;

pub(crate) const TYPE_LAYOUT_META_KIND: u64 = 0x5459_5045_4c41_594f; // "TYPELAYO"
pub(crate) const METADATA_DESC_START_BLOCK: u32 = 1;
pub(crate) const METADATA_DESC_BLOCK_COUNT: u32 = 1;
pub(crate) const TYPE_LAYOUT_METADATA_START_BLOCK: u32 = 2;
pub(crate) const TYPE_LAYOUT_METADATA_BLOCK_COUNT: u32 = 1;
pub(crate) const RESERVED_METADATA_BLOCKS: usize = 3;
pub(crate) const TYPE_LAYOUT_METADATA_HEADER_LEN: usize = 4;

pub(crate) fn empty_type_layout_metadata_block() -> Vec<u8> {
    let mut bytes = vec![0; TYPE_LAYOUT_METADATA_BLOCK_COUNT as usize * BLOCK_SIZE];
    bytes[..TYPE_LAYOUT_METADATA_HEADER_LEN].copy_from_slice(&0u32.to_le_bytes());
    bytes
}

pub(crate) fn load_type_layout_registry_from_block(bytes: &[u8]) -> Result<TypeLayoutRegistry> {
    ensure!(
        bytes.len() >= TYPE_LAYOUT_METADATA_HEADER_LEN,
        "transactional type layout metadata block is shorter than the header"
    );
    let payload_len = u32::from_le_bytes(
        bytes[..TYPE_LAYOUT_METADATA_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    let payload_len = usize::try_from(payload_len)
        .context("transactional type layout metadata payload length overflow")?;
    ensure!(
        payload_len <= bytes.len() - TYPE_LAYOUT_METADATA_HEADER_LEN,
        "transactional type layout metadata payload exceeds block capacity"
    );
    TypeLayoutRegistry::decode_all(
        &bytes[TYPE_LAYOUT_METADATA_HEADER_LEN..TYPE_LAYOUT_METADATA_HEADER_LEN + payload_len],
    )
}

pub(crate) fn append_type_layout_to_block(
    bytes: &mut [u8],
    layout: &PersistentTypeLayout,
) -> Result<bool> {
    ensure!(
        bytes.len() >= TYPE_LAYOUT_METADATA_HEADER_LEN,
        "transactional type layout metadata block is shorter than the header"
    );
    let payload_len = u32::from_le_bytes(
        bytes[..TYPE_LAYOUT_METADATA_HEADER_LEN]
            .try_into()
            .unwrap(),
    );
    let payload_len = usize::try_from(payload_len)
        .context("transactional type layout metadata payload length overflow")?;
    ensure!(
        payload_len <= bytes.len() - TYPE_LAYOUT_METADATA_HEADER_LEN,
        "transactional type layout metadata payload exceeds block capacity"
    );

    let mut registry = TypeLayoutRegistry::decode_all(
        &bytes[TYPE_LAYOUT_METADATA_HEADER_LEN..TYPE_LAYOUT_METADATA_HEADER_LEN + payload_len],
    )?;
    if let Some(existing) = registry.get(layout.id()) {
        if existing == layout {
            return Ok(false);
        }
    }
    registry.insert(layout.clone())?;

    let encoded = layout.encode()?;
    let payload_end = TYPE_LAYOUT_METADATA_HEADER_LEN
        .checked_add(payload_len)
        .context("transactional type layout metadata offset overflow")?;
    let new_payload_len = payload_len
        .checked_add(encoded.len())
        .context("transactional type layout metadata payload length overflow")?;
    ensure!(
        new_payload_len <= bytes.len() - TYPE_LAYOUT_METADATA_HEADER_LEN,
        "transactional type layout metadata block is full"
    );
    let new_end = payload_end
        .checked_add(encoded.len())
        .context("transactional type layout metadata write overflow")?;
    bytes[payload_end..new_end].copy_from_slice(&encoded);
    bytes[new_end..].fill(0);
    bytes[..TYPE_LAYOUT_METADATA_HEADER_LEN]
        .copy_from_slice(&(u32::try_from(new_payload_len).unwrap()).to_le_bytes());
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{
        TYPE_LAYOUT_METADATA_HEADER_LEN, append_type_layout_to_block,
        empty_type_layout_metadata_block, load_type_layout_registry_from_block,
    };
    use crate::runtime::transaction::type_layout::{
        PersistentTypeKind, PersistentTypeLayout, StructTraceField, TraceSlotKind, TypeLayoutId,
    };
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn sample_struct_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
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
                    value_size: 4,
                    kind: TraceSlotKind::Scalar,
                },
            ],
        }
    }

    fn sample_scalar_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        PersistentTypeLayout::Scalar {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
            kind: PersistentTypeKind::Extern,
        }
    }

    fn sample_large_struct_layout(id: u32, fingerprint: u64) -> PersistentTypeLayout {
        let fields = (0..4093)
            .map(|index| StructTraceField {
                field_index: index,
                field_offset: index.checked_mul(8).unwrap(),
                value_size: 8,
                kind: TraceSlotKind::Scalar,
            })
            .collect();
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(id).unwrap(),
            fingerprint,
            body_size: 4093 * 8,
            fields,
        }
    }

    #[test]
    fn empty_metadata_block_loads_empty_registry() {
        let bytes = empty_type_layout_metadata_block();

        let registry = load_type_layout_registry_from_block(&bytes).unwrap();

        assert_eq!(registry.iter().count(), 0);
    }

    #[test]
    fn append_and_load_round_trip_multiple_layouts() {
        let first = sample_struct_layout(11, 0x1111_2222_3333_4444);
        let second = sample_scalar_layout(12, 0x5555_6666_7777_8888);
        let mut bytes = empty_type_layout_metadata_block();

        assert!(append_type_layout_to_block(&mut bytes, &first).unwrap());
        assert!(append_type_layout_to_block(&mut bytes, &second).unwrap());

        let registry = load_type_layout_registry_from_block(&bytes).unwrap();
        let layouts = registry.iter().cloned().collect::<Vec<_>>();

        assert_eq!(layouts, vec![first, second]);
    }

    #[test]
    fn duplicate_layout_append_is_idempotent() {
        let layout = sample_struct_layout(21, 0x9999_aaaa_bbbb_cccc);
        let mut bytes = empty_type_layout_metadata_block();

        assert!(append_type_layout_to_block(&mut bytes, &layout).unwrap());
        assert!(!append_type_layout_to_block(&mut bytes, &layout).unwrap());

        let payload_len = u32::from_le_bytes(
            bytes[..TYPE_LAYOUT_METADATA_HEADER_LEN].try_into().unwrap(),
        );
        assert_eq!(usize::try_from(payload_len).unwrap(), layout.encode().unwrap().len());
    }

    #[test]
    fn corrupt_layout_metadata_record_is_rejected() {
        let layout = sample_struct_layout(31, 0x0123_4567_89ab_cdef);
        let mut bytes = empty_type_layout_metadata_block();
        append_type_layout_to_block(&mut bytes, &layout).unwrap();

        bytes[TYPE_LAYOUT_METADATA_HEADER_LEN + 12..TYPE_LAYOUT_METADATA_HEADER_LEN + 14]
            .copy_from_slice(&0xffff_u16.to_le_bytes());

        let err = load_type_layout_registry_from_block(&bytes)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown persistent type kind"));
    }

    #[test]
    fn append_overflow_is_rejected_without_mutating_block() {
        let mut bytes = empty_type_layout_metadata_block();
        for index in 0..8u32 {
            let layout =
                sample_large_struct_layout(100 + index, 0x1000_0000_0000_0000 + u64::from(index));
            assert!(append_type_layout_to_block(&mut bytes, &layout).unwrap());
        }

        let before = bytes.clone();
        let overflow = sample_large_struct_layout(999, 0xfedc_ba98_7654_3210);

        let err = append_type_layout_to_block(&mut bytes, &overflow)
            .unwrap_err()
            .to_string();

        assert!(err.contains("metadata block is full"));
        assert_eq!(bytes, before);
    }
}
