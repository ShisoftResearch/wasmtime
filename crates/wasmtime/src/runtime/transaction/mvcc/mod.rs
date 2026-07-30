use super::visibility::TransactionVisibility;

/// Feature-selected placeholder for future multiversion visibility state.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MvccVisibility;

impl MvccVisibility {
    pub(crate) const fn is_multiversion(&self) -> bool {
        true
    }
}

impl TransactionVisibility for MvccVisibility {
    type Snapshot = ();
}
