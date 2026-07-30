/// Compile-selected transaction visibility policy.
pub(crate) trait TransactionVisibility: Clone + core::fmt::Debug + Default {
    type Snapshot: core::fmt::Debug;
}

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
}

#[cfg(not(feature = "transaction-mvcc"))]
pub(crate) type SelectedTransactionVisibility = SingleVersionVisibility;

#[cfg(feature = "transaction-mvcc")]
pub(crate) type SelectedTransactionVisibility = super::mvcc::MvccVisibility;

pub(crate) type SelectedVisibilitySnapshot =
    <SelectedTransactionVisibility as TransactionVisibility>::Snapshot;
