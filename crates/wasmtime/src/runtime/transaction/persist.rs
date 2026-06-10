use crate::prelude::*;
#[cfg(test)]
use crate::runtime::vm::unpack_object_granule_id;
use crate::runtime::vm::{PackedGranuleDomain, TMemory, pack_object_granule_id};
use alloc::vec::Vec;

#[derive(Debug, Clone)]
pub(crate) struct PendingPublication {
    pub(crate) logical_id: u64,
    pub(crate) version: u32,
    pub(crate) kind: u16,
    pub(crate) type_info: u32,
    pub(crate) payload: Vec<u8>,
}

pub(crate) fn encode_data_record(pub_: &PendingPublication) -> Result<Vec<u8>> {
    TMemory::encode_publication_data_record(
        pub_.logical_id,
        pub_.version,
        pub_.kind,
        pub_.type_info,
        &pub_.payload,
    )
}

impl PendingPublication {
    pub(crate) fn persistent_object(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_info: u32,
        payload: Vec<u8>,
    ) -> Result<Self> {
        Ok(Self {
            logical_id: pack_object_granule_id(domain, object_id)?,
            version,
            kind: domain as u16,
            type_info,
            payload,
        })
    }
}

#[cfg(test)]
impl PendingPublication {
    fn tmemory_for_test(logical_id: u64, version: u32, payload: &[u8]) -> Self {
        Self {
            logical_id,
            version,
            kind: 1,
            type_info: 0,
            payload: payload.to_vec(),
        }
    }

    fn persistent_object_for_test(
        domain: PackedGranuleDomain,
        object_id: u64,
        version: u32,
        type_info: u32,
        payload: &[u8],
    ) -> Self {
        Self::persistent_object(domain, object_id, version, type_info, payload.to_vec()).unwrap()
    }
}

pub(crate) trait DurableSink {
    fn append_data_record(&mut self, record: &[u8]) -> Result<(u32, u32)>;
    fn append_log_entry(
        &mut self,
        logical_id: u64,
        version: u32,
        tx_meta: u32,
        data_block: u32,
        data_offset: u32,
        is_final: bool,
    ) -> Result<()>;
    fn flush_data(&mut self) -> Result<()>;
    fn flush_log(&mut self) -> Result<()>;
    fn fence(&mut self) -> Result<()>;
}

pub(crate) struct StreamPublisher<'a, S> {
    sink: &'a mut S,
    stream_id: u32,
    txid: u32,
}

impl<'a, S> StreamPublisher<'a, S>
where
    S: DurableSink,
{
    pub(crate) fn new(sink: &'a mut S, stream_id: u32, txid: u32) -> Self {
        Self {
            sink,
            stream_id,
            txid,
        }
    }

    pub(crate) fn publish_transaction(&mut self, pubs: &[PendingPublication]) -> Result<()> {
        let _ = self.stream_id;

        let mut ordinary = Vec::new();
        for pub_ in pubs {
            let record = encode_data_record(pub_)?;
            let (data_block, data_offset) = self.sink.append_data_record(&record)?;
            ordinary.push((
                pub_.logical_id,
                pub_.version,
                self.txid << 1,
                data_block,
                data_offset,
            ));
        }

        self.sink.flush_data()?;
        self.sink.fence()?;

        for &(logical_id, version, tx_meta, data_block, data_offset) in
            ordinary.iter().take(ordinary.len().saturating_sub(1))
        {
            self.sink.append_log_entry(
                logical_id,
                version,
                tx_meta,
                data_block,
                data_offset,
                false,
            )?;
        }
        self.sink.flush_log()?;

        if let Some(&(logical_id, version, tx_meta, data_block, data_offset)) = ordinary.last() {
            self.sink.append_log_entry(
                logical_id,
                version,
                tx_meta,
                data_block,
                data_offset,
                true,
            )?;
            self.sink.flush_log()?;
            self.sink.fence()?;
        }

        Ok(())
    }

    pub(crate) fn abort_transaction(&mut self, _pubs: &[PendingPublication]) -> Result<()> {
        let _ = self.stream_id;
        Ok(())
    }
}

#[cfg(test)]
impl<'a, S> StreamPublisher<'a, S>
where
    S: DurableSink,
{
    fn new_for_test(sink: &'a mut S, stream_id: u32, txid: u32) -> Self {
        Self::new(sink, stream_id, txid)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DurabilityEvent {
    DataWrite,
    DataFlush,
    LogWrite,
    LogFlush,
    Fence,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct RecordingDurability {
    events: Vec<DurabilityEvent>,
    tx_meta: Vec<u32>,
    next_data_block: u32,
}

#[cfg(test)]
impl RecordingDurability {
    fn final_lp_count(&self) -> usize {
        self.tx_meta
            .iter()
            .filter(|entry| (**entry & 1) != 0)
            .count()
    }

    fn non_final_log_entries_before_final_lp(&self) -> bool {
        let Some(final_index) = self.tx_meta.iter().position(|entry| (*entry & 1) != 0) else {
            return false;
        };
        self.tx_meta[..final_index]
            .iter()
            .all(|entry| (*entry & 1) == 0)
    }

    fn events(&self) -> &[DurabilityEvent] {
        &self.events
    }
}

#[cfg(test)]
impl DurableSink for RecordingDurability {
    fn append_data_record(&mut self, _record: &[u8]) -> Result<(u32, u32)> {
        if self.events.last() != Some(&DurabilityEvent::DataWrite) {
            self.events.push(DurabilityEvent::DataWrite);
        }
        let data_block = self.next_data_block;
        self.next_data_block = self
            .next_data_block
            .checked_add(1)
            .context("recording durability data block overflow")?;
        Ok((data_block, 0))
    }

    fn append_log_entry(
        &mut self,
        _logical_id: u64,
        _version: u32,
        tx_meta: u32,
        _data_block: u32,
        _data_offset: u32,
        is_final: bool,
    ) -> Result<()> {
        self.events.push(DurabilityEvent::LogWrite);
        let stored_tx_meta = if is_final { tx_meta | 1 } else { tx_meta & !1 };
        self.tx_meta.push(stored_tx_meta);
        Ok(())
    }

    fn flush_data(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::DataFlush);
        Ok(())
    }

    fn flush_log(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::LogFlush);
        Ok(())
    }

    fn fence(&mut self) -> Result<()> {
        self.events.push(DurabilityEvent::Fence);
        Ok(())
    }
}

#[cfg(test)]
fn sample_publications() -> Vec<PendingPublication> {
    vec![
        PendingPublication {
            logical_id: 0x1000,
            version: 1,
            kind: 7,
            type_info: 0x10,
            payload: vec![1, 2, 3, 4],
        },
        PendingPublication {
            logical_id: 0x1001,
            version: 2,
            kind: 9,
            type_info: 0x20,
            payload: vec![5, 6, 7, 8, 9],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{
        ObjectPayload, ObjectValue, object_heap::TxObjectHeader,
        object_heap::encode_object_record_for_test,
    };

    #[test]
    fn commit_publishes_only_the_last_entry_with_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 21);
        publisher
            .publish_transaction(&sample_publications())
            .unwrap();

        assert_eq!(recorder.final_lp_count(), 1);
        assert!(recorder.non_final_log_entries_before_final_lp());
    }

    #[test]
    fn commit_orders_data_before_final_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 4, 99);
        publisher
            .publish_transaction(&sample_publications())
            .unwrap();

        assert!(recorder.events().starts_with(&[
            DurabilityEvent::DataWrite,
            DurabilityEvent::DataFlush,
            DurabilityEvent::Fence,
        ]));
        assert_eq!(recorder.events().last(), Some(&DurabilityEvent::Fence));
    }

    #[test]
    fn abort_does_not_publish_lp() {
        let mut recorder = RecordingDurability::default();
        let mut publisher = StreamPublisher::new_for_test(&mut recorder, 5, 12);
        publisher.abort_transaction(&sample_publications()).unwrap();
        assert_eq!(recorder.final_lp_count(), 0);
    }

    #[test]
    fn encodes_tmemory_granule_record() {
        let publication =
            PendingPublication::tmemory_for_test(0x1000_0000_0000_0001, 5, &[1, 2, 3, 4]);
        let bytes = encode_data_record(&publication).unwrap();
        assert_eq!(u16::from_le_bytes(bytes[12..14].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 5);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 4);
    }

    #[test]
    fn encodes_object_record_with_tx_object_header() {
        let bytes = encode_object_record_for_test(
            41,
            7,
            12,
            &ObjectPayload::Struct(vec![ObjectValue::I32(9)]),
        )
        .unwrap();
        let header = TxObjectHeader::read_from_prefix(&bytes).unwrap();
        assert_eq!(header.object_id, 41);
        assert_eq!(header.version, 7);
        assert_eq!(header.type_index, 12);
    }

    #[test]
    fn object_publication_builds_object_logical_id() {
        let pub_ = PendingPublication::persistent_object_for_test(
            PackedGranuleDomain::TStruct,
            41,
            7,
            12,
            &[1, 2, 3],
        );
        let (domain, object_id) = unpack_object_granule_id(pub_.logical_id).unwrap();
        assert_eq!(domain, PackedGranuleDomain::TStruct);
        assert_eq!(object_id, 41);
        assert_eq!(pub_.version, 7);
        assert_eq!(pub_.type_info, 12);
    }
}
