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
}

#[derive(Debug, Default)]
pub(crate) struct MvccCertificationAuthority {
    state: Mutex<CertificationState>,
    changed: Condvar,
}

impl MvccCertificationAuthority {
    pub(crate) fn acquire(
        self: &Arc<Self>,
        transaction: TransactionId,
        reads: &BTreeSet<GranuleId>,
        writes: &BTreeSet<GranuleId>,
    ) -> Result<MvccCertificationPermit> {
        let reservations = sorted_reservations(reads, writes);
        let mut state = self.lock()?;
        while !reservations.iter().all(|(granule, mode)| {
            state
                .granules
                .get(granule)
                .is_none_or(|latch| latch.is_compatible(*mode))
        }) {
            state = self
                .changed
                .wait(state)
                .map_err(|_| crate::format_err!("MVCC certification authority lock is poisoned"))?;
        }

        for (granule, mode) in reservations.iter().copied() {
            state
                .granules
                .entry(granule)
                .or_default()
                .reserve(transaction, mode);
        }
        drop(state);
        Ok(MvccCertificationPermit {
            authority: self.clone(),
            transaction,
            reservations,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, CertificationState>> {
        self.state
            .lock()
            .map_err(|_| crate::format_err!("MVCC certification authority lock is poisoned"))
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
pub(crate) struct MvccCertificationPermit {
    authority: Arc<MvccCertificationAuthority>,
    transaction: TransactionId,
    reservations: Vec<(GranuleId, CertificationMode)>,
}

impl MvccCertificationPermit {
    #[cfg(test)]
    pub(crate) fn reservations_for_test(&self) -> &[(GranuleId, CertificationMode)] {
        &self.reservations
    }
}

impl Drop for MvccCertificationPermit {
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
        drop(state);
        self.authority.changed.notify_all();
    }
}
