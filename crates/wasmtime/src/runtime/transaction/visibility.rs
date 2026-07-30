use super::{GlobalSnapshot, GranuleId, ObjectId, ObjectPayload, TableGranuleSnapshot};
use crate::prelude::*;

/// Compile-selected transaction visibility policy.
pub(crate) trait TransactionVisibility: Clone + core::fmt::Debug + Default {
    type Snapshot: core::fmt::Debug;

    fn begin_snapshot(&self) -> Result<Self::Snapshot>;
    fn snapshot_timestamp(snapshot: &Self::Snapshot) -> CommitTimestamp;
    fn finish_snapshot(&self, snapshot: Self::Snapshot) -> Result<()>;

    fn read_memory<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>;

    fn read_memory_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>;

    fn read_global<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<GlobalSnapshot>
    where
        F: FnOnce() -> Result<GlobalSnapshot>;

    fn read_table<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<TableGranuleSnapshot>
    where
        F: FnOnce() -> Result<TableGranuleSnapshot>;

    fn read_table_size<F>(
        &self,
        snapshot: CommitTimestamp,
        granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>;

    fn read_object<F>(
        &self,
        snapshot: CommitTimestamp,
        object: ObjectId,
        current: F,
    ) -> Result<Option<ObjectPayload>>
    where
        F: FnOnce() -> Result<Option<ObjectPayload>>;
}

/// A timestamp assigned to an MVCC commit.
pub(crate) type CommitTimestamp = u64;

/// The immutable timestamp assigned to versions that predate MVCC commits.
pub(crate) const BASELINE_TIMESTAMP: CommitTimestamp = 0;

/// Single-version visibility passes through to the current transaction state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SingleVersionVisibility;

impl SingleVersionVisibility {
    pub(crate) const fn is_multiversion(&self) -> bool {
        false
    }
}

impl TransactionVisibility for SingleVersionVisibility {
    type Snapshot = ();

    fn begin_snapshot(&self) -> Result<Self::Snapshot> {
        Ok(())
    }

    fn snapshot_timestamp(_snapshot: &Self::Snapshot) -> CommitTimestamp {
        BASELINE_TIMESTAMP
    }

    fn finish_snapshot(&self, _snapshot: Self::Snapshot) -> Result<()> {
        Ok(())
    }

    fn read_memory<F>(
        &self,
        _snapshot: CommitTimestamp,
        _granule: GranuleId,
        current: F,
    ) -> Result<Vec<u8>>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        current()
    }

    fn read_memory_size<F>(
        &self,
        _snapshot: CommitTimestamp,
        _granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>,
    {
        current()
    }

    fn read_global<F>(
        &self,
        _snapshot: CommitTimestamp,
        _granule: GranuleId,
        current: F,
    ) -> Result<GlobalSnapshot>
    where
        F: FnOnce() -> Result<GlobalSnapshot>,
    {
        current()
    }

    fn read_table<F>(
        &self,
        _snapshot: CommitTimestamp,
        _granule: GranuleId,
        current: F,
    ) -> Result<TableGranuleSnapshot>
    where
        F: FnOnce() -> Result<TableGranuleSnapshot>,
    {
        current()
    }

    fn read_table_size<F>(
        &self,
        _snapshot: CommitTimestamp,
        _granule: GranuleId,
        current: F,
    ) -> Result<u64>
    where
        F: FnOnce() -> Result<u64>,
    {
        current()
    }

    fn read_object<F>(
        &self,
        _snapshot: CommitTimestamp,
        _object: ObjectId,
        current: F,
    ) -> Result<Option<ObjectPayload>>
    where
        F: FnOnce() -> Result<Option<ObjectPayload>>,
    {
        current()
    }
}

#[cfg(not(feature = "transaction-mvcc"))]
pub(crate) type SelectedTransactionVisibility = SingleVersionVisibility;

#[cfg(feature = "transaction-mvcc")]
pub(crate) type SelectedTransactionVisibility = super::mvcc::MvccVisibility;

pub(crate) type SelectedVisibilitySnapshot =
    <SelectedTransactionVisibility as TransactionVisibility>::Snapshot;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::transaction::{
        GlobalSnapshot, GranuleId, ObjectId, ObjectPayload, ObjectValue, TableElementSnapshot,
        TableGranuleSnapshot,
    };

    #[test]
    fn single_version_visibility_passes_all_typed_reads_to_current_state() {
        let visibility = SingleVersionVisibility;
        let granule = GranuleId::TGlobal {
            instance: None,
            global_index: 0,
        };
        let table = TableGranuleSnapshot::new(vec![TableElementSnapshot::FuncRef(4)]).unwrap();
        let object = ObjectPayload::Struct(vec![ObjectValue::I32(5)]);

        assert_eq!(
            visibility.read_memory(99, granule, || Ok(vec![1])).unwrap(),
            vec![1]
        );
        assert_eq!(
            visibility.read_memory_size(99, granule, || Ok(2)).unwrap(),
            2
        );
        assert_eq!(
            visibility
                .read_global(99, granule, || Ok(GlobalSnapshot::I32(3)))
                .unwrap(),
            GlobalSnapshot::I32(3)
        );
        assert_eq!(
            visibility
                .read_table(99, granule, || Ok(table.clone()))
                .unwrap(),
            table
        );
        assert_eq!(
            visibility.read_table_size(99, granule, || Ok(4)).unwrap(),
            4
        );
        assert_eq!(
            visibility
                .read_object(99, ObjectId { object_index: 6 }, || Ok(Some(
                    object.clone()
                )))
                .unwrap(),
            Some(object)
        );
    }
}
