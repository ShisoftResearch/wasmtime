use super::super::{GranuleId, TransactionId};
use crate::prelude::*;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CertificationMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Default)]
struct LatchState {
    readers: BTreeSet<TransactionId>,
    writer: Option<TransactionId>,
}

impl LatchState {
    fn is_compatible(&self, mode: CertificationMode) -> bool {
        match mode {
            CertificationMode::Shared => self.writer.is_none(),
            CertificationMode::Exclusive => self.writer.is_none() && self.readers.is_empty(),
        }
    }

    fn reserve(&mut self, transaction: TransactionId, mode: CertificationMode) {
        match mode {
            CertificationMode::Shared => {
                let inserted = self.readers.insert(transaction);
                debug_assert!(inserted);
            }
            CertificationMode::Exclusive => {
                debug_assert!(self.writer.is_none());
                self.writer = Some(transaction);
            }
        }
    }

    fn release(&mut self, transaction: TransactionId, mode: CertificationMode) {
        match mode {
            CertificationMode::Shared => {
                let removed = self.readers.remove(&transaction);
                debug_assert!(removed);
            }
            CertificationMode::Exclusive => {
                debug_assert_eq!(self.writer, Some(transaction));
                if self.writer == Some(transaction) {
                    self.writer = None;
                }
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.readers.is_empty() && self.writer.is_none()
    }
}

#[derive(Debug, Default)]
struct CertificationState {
    granules: BTreeMap<GranuleId, LatchState>,
    #[cfg(test)]
    blocked_transactions: BTreeSet<TransactionId>,
    #[cfg(test)]
    acquisitions: usize,
    #[cfg(test)]
    active_permits: usize,
    #[cfg(test)]
    last_reservations: Vec<(GranuleId, CertificationMode)>,
}

#[derive(Debug, Default)]
pub(crate) struct OptimisticCertificationAuthority {
    state: Mutex<CertificationState>,
    changed: Condvar,
}

impl OptimisticCertificationAuthority {
    pub(crate) fn acquire(
        self: &Arc<Self>,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<OptimisticCertificationPermit> {
        let reservations = sorted_reservations(reads, writes);
        let mut state = self.lock()?;
        while !reservations.iter().all(|(granule, mode)| {
            state
                .granules
                .get(granule)
                .is_none_or(|latch| latch.is_compatible(*mode))
        }) {
            #[cfg(test)]
            {
                state.blocked_transactions.insert(transaction);
                self.changed.notify_all();
            }
            state = self.changed.wait(state).map_err(|_| {
                crate::format_err!("optimistic certification authority lock is poisoned")
            })?;
            #[cfg(test)]
            state.blocked_transactions.remove(&transaction);
        }

        for (granule, mode) in reservations.iter().copied() {
            state
                .granules
                .entry(granule)
                .or_default()
                .reserve(transaction, mode);
        }
        #[cfg(test)]
        {
            state.acquisitions += 1;
            state.active_permits += 1;
            state.last_reservations = reservations.clone();
        }
        drop(state);
        Ok(OptimisticCertificationPermit {
            authority: self.clone(),
            transaction,
            reservations,
        })
    }

    #[cfg(test)]
    pub(crate) fn wait_until_blocked_for_test(
        &self,
        transaction: TransactionId,
        timeout: std::time::Duration,
    ) -> Result<bool> {
        let state = self.lock()?;
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.blocked_transactions.contains(&transaction)
            })
            .map_err(|_| {
                crate::format_err!("optimistic certification authority lock is poisoned")
            })?;
        Ok(state.blocked_transactions.contains(&transaction))
    }

    #[cfg(test)]
    pub(crate) fn lifecycle_counts_for_test(&self) -> Result<(usize, usize)> {
        let state = self.lock()?;
        Ok((state.acquisitions, state.active_permits))
    }

    #[cfg(test)]
    pub(crate) fn last_reservations_for_test(&self) -> Result<Vec<(GranuleId, CertificationMode)>> {
        Ok(self.lock()?.last_reservations.clone())
    }

    fn lock(&self) -> Result<MutexGuard<'_, CertificationState>> {
        self.state
            .lock()
            .map_err(|_| crate::format_err!("optimistic certification authority lock is poisoned"))
    }
}

fn sorted_reservations(
    reads: &BTreeSet<GranuleId>,
    writes: &BTreeSet<GranuleId>,
) -> Vec<(GranuleId, CertificationMode)> {
    let mut reservations = BTreeMap::new();
    for granule in reads.iter().copied() {
        reservations.insert(granule, CertificationMode::Shared);
    }
    for granule in writes.iter().copied() {
        reservations.insert(granule, CertificationMode::Exclusive);
    }
    reservations.into_iter().collect()
}

#[derive(Debug)]
pub(crate) struct OptimisticCertificationPermit {
    authority: Arc<OptimisticCertificationAuthority>,
    transaction: TransactionId,
    reservations: Vec<(GranuleId, CertificationMode)>,
}

impl OptimisticCertificationPermit {
    #[cfg(test)]
    pub(crate) fn reservations_for_test(&self) -> &[(GranuleId, CertificationMode)] {
        &self.reservations
    }
}

impl Drop for OptimisticCertificationPermit {
    fn drop(&mut self) {
        let mut state = match self.authority.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        for (granule, mode) in self.reservations.iter().copied() {
            let remove = {
                let latch = state
                    .granules
                    .get_mut(&granule)
                    .expect("certification permit reservation must exist");
                latch.release(self.transaction, mode);
                latch.is_empty()
            };
            if remove {
                state.granules.remove(&granule);
            }
        }
        #[cfg(test)]
        {
            state.active_permits -= 1;
        }
        drop(state);
        self.authority.changed.notify_all();
    }
}
