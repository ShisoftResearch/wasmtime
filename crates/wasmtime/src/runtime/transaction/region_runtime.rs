use crate::prelude::*;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{LockBased, TxDurableLog};

#[derive(Clone, Debug)]
pub(crate) struct TransactionRegionRuntime(Arc<Mutex<TransactionRegionRuntimeInner>>);

#[derive(Debug, Default)]
pub(crate) struct TransactionRegionRuntimeInner {
    pub(crate) locks: LockBased,
    pub(crate) durable_log: TxDurableLog,
}

impl Default for TransactionRegionRuntime {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(
            TransactionRegionRuntimeInner::default(),
        )))
    }
}

impl TransactionRegionRuntime {
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, TransactionRegionRuntimeInner>> {
        self.0
            .lock()
            .map_err(|_| crate::format_err!("transaction region runtime lock poisoned"))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
