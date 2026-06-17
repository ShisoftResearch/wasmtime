use core::cell::Cell;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct TransactionId(pub(crate) u64);

std::thread_local! {
    static CURRENT_TRANSACTION: Cell<Option<TransactionId>> = const { Cell::new(None) };
}

pub(super) fn current_thread_transaction() -> Option<TransactionId> {
    CURRENT_TRANSACTION.with(Cell::get)
}

pub(super) fn replace_current_thread_transaction(
    transaction: Option<TransactionId>,
) -> Option<TransactionId> {
    CURRENT_TRANSACTION.with(|current| {
        let previous = current.get();
        current.set(transaction);
        previous
    })
}

#[cfg(test)]
pub(super) fn current_thread_transaction_for_test() -> Option<TransactionId> {
    current_thread_transaction()
}

#[cfg(test)]
pub(super) fn clear_current_thread_transaction_for_test() {
    replace_current_thread_transaction(None);
}

#[cfg(test)]
pub(super) struct ClearCurrentThreadTransactionOnDrop;

#[cfg(test)]
impl Drop for ClearCurrentThreadTransactionOnDrop {
    fn drop(&mut self) {
        clear_current_thread_transaction_for_test();
    }
}

#[cfg(test)]
pub(super) fn clear_current_thread_transaction_on_drop_for_test()
-> ClearCurrentThreadTransactionOnDrop {
    ClearCurrentThreadTransactionOnDrop
}

impl TransactionId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ObjectId {
    pub(crate) object_index: u64,
}
