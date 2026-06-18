use crate::prelude::*;
use std::sync::{Arc, Mutex, MutexGuard};

use super::{LockBased, TransactionId, TxDurableLog};

#[derive(Clone, Debug)]
pub(crate) struct TransactionRegionRuntime(Arc<Mutex<TransactionRegionRuntimeInner>>);

#[derive(Debug)]
pub(crate) struct TransactionRegionRuntimeInner {
    pub(crate) next_transaction_id: u64,
    pub(crate) locks: LockBased,
    pub(crate) durable_log: TxDurableLog,
}

impl Default for TransactionRegionRuntimeInner {
    fn default() -> Self {
        Self {
            next_transaction_id: 10_001,
            locks: LockBased::default(),
            durable_log: TxDurableLog::default(),
        }
    }
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

    pub(crate) fn allocate_transaction_id(&self) -> Result<TransactionId> {
        let mut runtime = self.lock()?;
        let id = TransactionId::from_raw(runtime.next_transaction_id);
        runtime.next_transaction_id = runtime
            .next_transaction_id
            .checked_add(1)
            .context("transaction id overflow")?;
        Ok(id)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn allocate_transaction_id_for_test(&self) -> Result<TransactionId> {
        self.allocate_transaction_id()
    }

    #[cfg(test)]
    pub(crate) fn ptr_eq_for_test(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
