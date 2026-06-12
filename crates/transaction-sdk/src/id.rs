#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PersistentIdKind {
    TMemory = 0,
    Object = 1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub struct PersistentId(u64);

impl PersistentId {
    pub const KIND_BITS: u64 = 4;
    pub const PAYLOAD_BITS: u64 = 60;
    pub const PAYLOAD_MASK: u64 = (1u64 << Self::PAYLOAD_BITS) - 1;

    pub const fn tmemory_offset(offset: u32) -> Self {
        Self(((PersistentIdKind::TMemory as u64) << Self::PAYLOAD_BITS) | offset as u64)
    }

    pub const fn object_id(object_id: u64) -> Self {
        Self(
            ((PersistentIdKind::Object as u64) << Self::PAYLOAD_BITS)
                | (object_id & Self::PAYLOAD_MASK),
        )
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn payload(self) -> u64 {
        self.0 & Self::PAYLOAD_MASK
    }

    pub const fn kind(self) -> PersistentIdKind {
        match self.0 >> Self::PAYLOAD_BITS {
            0 => PersistentIdKind::TMemory,
            1 => PersistentIdKind::Object,
            _ => PersistentIdKind::Object,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmemory_id_encodes_kind_and_offset() {
        let id = PersistentId::tmemory_offset(128);
        assert_eq!(id.kind(), PersistentIdKind::TMemory);
        assert_eq!(id.payload(), 128);
        assert_eq!(id.raw() & PersistentId::PAYLOAD_MASK, 128);
    }

    #[test]
    fn object_id_encodes_kind_and_payload() {
        let id = PersistentId::object_id(42);
        assert_eq!(id.kind(), PersistentIdKind::Object);
        assert_eq!(id.payload(), 42);
    }
}
