use super::super::visibility::CommitTimestamp;
use super::{
    MvccGcBarrierPermit, MvccGcMetadata, MvccRuntime, ObjectId, ObjectPayload, VersionChain,
};
use crate::prelude::*;
use alloc::collections::BTreeMap;
use core::ops::Bound::{Excluded, Unbounded};

const VERSION_TABLE_COUNT: usize = 6;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MvccPruneBudget {
    chains: usize,
}

impl MvccPruneBudget {
    pub(crate) const fn chains(chains: usize) -> Self {
        Self { chains }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MvccPruneReport {
    pub(crate) scanned_chains: usize,
    pub(crate) removed_versions: usize,
    pub(crate) removed_chains: usize,
    pub(crate) scanned_by_table: [usize; VERSION_TABLE_COUNT],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MvccExpectedObjectState {
    Present {
        object: ObjectId,
        payload: ObjectPayload,
    },
    Absent {
        object: ObjectId,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MvccRebasePlan {
    metadata: MvccGcMetadata,
    expected_objects: Vec<MvccExpectedObjectState>,
}

impl MvccRebasePlan {
    pub(crate) fn metadata(&self) -> MvccGcMetadata {
        self.metadata
    }

    pub(crate) fn expected_objects(&self) -> &[MvccExpectedObjectState] {
        &self.expected_objects
    }
}

impl MvccRuntime {
    pub(crate) fn prune_versions(&self, budget: MvccPruneBudget) -> Result<MvccPruneReport> {
        let horizon = self.coordinator.pruning_horizon()?;
        self.prune_versions_at_horizon(horizon, budget)
    }

    fn prune_versions_at_horizon(
        &self,
        horizon: CommitTimestamp,
        budget: MvccPruneBudget,
    ) -> Result<MvccPruneReport> {
        let mut state = self.domains.lock()?;
        let mut report = MvccPruneReport::default();
        for _ in 0..budget.chains {
            let mut scanned = false;
            for _ in 0..VERSION_TABLE_COUNT {
                let table = state.next_prune_table;
                state.next_prune_table = (state.next_prune_table + 1) % VERSION_TABLE_COUNT;
                let outcome = match table {
                    0 => {
                        let table = &mut state.memories;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    1 => {
                        let table = &mut state.memory_sizes;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    2 => {
                        let table = &mut state.globals;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    3 => {
                        let table = &mut state.tables;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    4 => {
                        let table = &mut state.table_sizes;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    5 => {
                        let table = &mut state.objects;
                        prune_one_typed_chain(&mut table.chains, &mut table.prune_cursor, horizon)
                    }
                    _ => unreachable!(),
                };
                let Some((removed_versions, removed_chain)) = outcome else {
                    continue;
                };
                report.scanned_chains += 1;
                report.removed_versions += removed_versions;
                report.removed_chains += usize::from(removed_chain);
                report.scanned_by_table[table] += 1;
                scanned = true;
                break;
            }
            if !scanned {
                break;
            }
        }
        Ok(report)
    }

    pub(crate) fn prepare_full_rebase(
        &self,
        permit: &MvccGcBarrierPermit,
    ) -> Result<MvccRebasePlan> {
        self.coordinator.with_gc_barrier(permit, |metadata| {
            #[cfg(test)]
            self.run_gc_rebase_before_domains_hook_for_test()?;
            let state = self.domains.lock()?;
            Ok(MvccRebasePlan {
                metadata,
                expected_objects: validate_rebase_state(&state)?,
            })
        })
    }

    pub(crate) fn complete_full_rebase(
        &self,
        permit: &MvccGcBarrierPermit,
    ) -> Result<MvccGcMetadata> {
        let (_, metadata) = self.coordinator.complete_gc_rebase(permit, |_| {
            #[cfg(test)]
            self.run_gc_rebase_before_domains_hook_for_test()?;
            let mut state = self.domains.lock()?;
            validate_rebase_state(&state)?;
            state.memories.chains.clear();
            state.memories.prune_cursor = None;
            state.memory_sizes.chains.clear();
            state.memory_sizes.prune_cursor = None;
            state.globals.chains.clear();
            state.globals.prune_cursor = None;
            state.tables.chains.clear();
            state.tables.prune_cursor = None;
            state.table_sizes.chains.clear();
            state.table_sizes.prune_cursor = None;
            state.objects.chains.clear();
            state.objects.prune_cursor = None;
            state.next_prune_table = 0;
            Ok(())
        })?;
        Ok(metadata)
    }

    #[cfg(test)]
    pub(crate) fn prune_horizon_for_test(&self) -> Result<CommitTimestamp> {
        self.coordinator.pruning_horizon()
    }

    #[cfg(test)]
    pub(crate) fn prune_versions_at_horizon_for_test(
        &self,
        horizon: CommitTimestamp,
        budget: MvccPruneBudget,
    ) -> Result<MvccPruneReport> {
        self.prune_versions_at_horizon(horizon, budget)
    }

    #[cfg(test)]
    pub(crate) fn prune_versions_for_test(
        &self,
        budget: MvccPruneBudget,
    ) -> Result<MvccPruneReport> {
        self.prune_versions(budget)
    }

    #[cfg(test)]
    pub(crate) fn total_version_chain_count_for_test(&self) -> Result<usize> {
        let state = self.domains.lock()?;
        Ok(state.memories.chains.len()
            + state.memory_sizes.chains.len()
            + state.globals.chains.len()
            + state.tables.chains.len()
            + state.table_sizes.chains.len()
            + state.objects.chains.len())
    }

    #[cfg(test)]
    pub(crate) fn prune_cursor_state_for_test(
        &self,
    ) -> Result<(usize, [Option<usize>; VERSION_TABLE_COUNT])> {
        let state = self.domains.lock()?;
        Ok((
            state.next_prune_table,
            [
                state.memories.prune_cursor.map(|_| 0),
                state.memory_sizes.prune_cursor.map(|_| 1),
                state.globals.prune_cursor.map(|_| 2),
                state.tables.prune_cursor.map(|_| 3),
                state.table_sizes.prune_cursor.map(|_| 4),
                state.objects.prune_cursor.map(|_| 5),
            ],
        ))
    }

    #[cfg(test)]
    pub(crate) fn coordinator_metadata_for_test(&self) -> Result<MvccGcMetadata> {
        self.coordinator.metadata()
    }

    #[cfg(test)]
    pub(crate) fn set_gc_rebase_before_domains_hook_for_test(
        &self,
        hook: Option<alloc::sync::Arc<super::domains::MvccCommitTestHook>>,
    ) -> Result<()> {
        *self
            .gc_rebase_before_domains
            .lock()
            .map_err(|_| crate::format_err!("MVCC GC rebase test hook lock is poisoned"))? = hook;
        Ok(())
    }

    #[cfg(test)]
    fn run_gc_rebase_before_domains_hook_for_test(&self) -> Result<()> {
        let hook = self
            .gc_rebase_before_domains
            .lock()
            .map_err(|_| crate::format_err!("MVCC GC rebase test hook lock is poisoned"))?
            .clone();
        if let Some(hook) = hook {
            hook.wait()?;
        }
        Ok(())
    }
}

fn validate_rebase_state(
    state: &super::domains::MvccDomainState,
) -> Result<Vec<MvccExpectedObjectState>> {
    validate_committed_chains(&state.memories.chains, "tmemory")?;
    validate_committed_chains(&state.memory_sizes.chains, "tmemory size")?;
    validate_committed_chains(&state.globals.chains, "global")?;
    validate_committed_chains(&state.tables.chains, "table")?;
    validate_committed_chains(&state.table_sizes.chains, "table size")?;

    let mut expected_objects = Vec::with_capacity(state.objects.chains.len());
    for (&object, chain) in &state.objects.chains {
        ensure!(
            !chain.has_pending_version(),
            "MVCC object chain has a pending version during GC rebase"
        );
        match chain.newest_committed_value() {
            Some(payload) => expected_objects.push(MvccExpectedObjectState::Present {
                object,
                payload: payload.clone(),
            }),
            None => expected_objects.push(MvccExpectedObjectState::Absent { object }),
        }
    }
    Ok(expected_objects)
}

fn validate_committed_chains<K, T>(
    chains: &BTreeMap<K, VersionChain<T>>,
    description: &str,
) -> Result<()> {
    for chain in chains.values() {
        ensure!(
            !chain.has_pending_version(),
            "MVCC {description} chain has a pending version during GC rebase"
        );
        ensure!(
            chain.newest_committed_value().is_some(),
            "MVCC {description} chain has no committed value during GC rebase"
        );
    }
    Ok(())
}

fn prune_one_typed_chain<K, T>(
    chains: &mut BTreeMap<K, VersionChain<T>>,
    cursor: &mut Option<K>,
    horizon: CommitTimestamp,
) -> Option<(usize, bool)>
where
    K: Copy + Ord,
{
    let key = next_chain_key(chains, *cursor)?;
    *cursor = Some(key);
    let removed_versions = chains.get_mut(&key).unwrap().prune_for_horizon(horizon);
    let removed_chain = chains.get(&key).unwrap().is_empty();
    if removed_chain {
        chains.remove(&key);
    }
    Some((removed_versions, removed_chain))
}

fn next_chain_key<K, T>(chains: &BTreeMap<K, T>, cursor: Option<K>) -> Option<K>
where
    K: Copy + Ord,
{
    cursor
        .and_then(|cursor| {
            chains
                .range((Excluded(cursor), Unbounded))
                .next()
                .map(|(key, _)| *key)
        })
        .or_else(|| chains.keys().next().copied())
}
