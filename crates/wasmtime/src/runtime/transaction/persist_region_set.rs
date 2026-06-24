#[cfg(test)]
use super::persist::MultiRegionRecoveryTimingForTest;
use super::persist::{
    DurableDataRecordPointer, DurableDataStream, DurableRegionLog, DurableRegionStorage,
    PendingCommitLogEntry, TxDurableLogBackend,
};
use super::type_layout::{PersistentTypeLayout, TypeLayoutId};
use crate::prelude::*;
#[cfg(test)]
use crate::runtime::transaction::config::TMemoryRegionConfig;
#[cfg(test)]
use crate::runtime::vm::cpus_for_node;
use crate::runtime::vm::{
    CpuSet, NumaNode, RecoveryOptions, RecoveryParallelism, TxLogEntry, current_cpu, node_for_cpu,
    pin_current_thread,
};
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::Mutex;

const REGION_RECOVERY_WORKERS: usize = 8;
static NEXT_MULTI_REGION_BACKEND_ID: AtomicUsize = AtomicUsize::new(1);

std::thread_local! {
    static THREAD_OWNED_REGION_OWNER: Cell<Option<ThreadOwnedRegionOwner>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RegionSelection {
    Pinned(usize),
    ThreadOwned,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct RegionPlacement {
    numa_node: Option<NumaNode>,
    cpu_set: Option<CpuSet>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ThreadOwnedRegionOwner {
    backend_id: usize,
    region_index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RegionBlockRoute {
    region_index: usize,
    region_local_block: u32,
    block_base: u32,
}

#[cfg(test)]
fn placements_for_regions(regions: &[TMemoryRegionConfig]) -> Result<Vec<RegionPlacement>> {
    regions
        .iter()
        .map(|region| {
            let numa_node = region.numa_node.map(NumaNode::new).transpose()?;
            let cpu_set = match numa_node {
                Some(node) => cpus_for_node(node)?,
                None => None,
            };
            Ok(RegionPlacement { numa_node, cpu_set })
        })
        .collect()
}

#[cfg(test)]
fn fixed_window_num_blocks(region: &TMemoryRegionConfig) -> Result<u32> {
    let blocks = region.reserved_len / crate::runtime::vm::block_region::BLOCK_SIZE;
    ensure!(
        blocks > 0,
        "multi-region durable log region {} reserved window is smaller than one block",
        region.path.display()
    );
    u32::try_from(blocks).context("multi-region durable log region block count overflow")
}

#[cfg(all(test, target_os = "linux"))]
fn fixed_window_for_region(
    region: &TMemoryRegionConfig,
) -> crate::runtime::vm::block_region::MappedAddressWindow {
    crate::runtime::vm::block_region::MappedAddressWindow {
        base: region.address_base,
        reserved_len: region.reserved_len,
    }
}

fn next_multi_region_backend_id() -> usize {
    let id = NEXT_MULTI_REGION_BACKEND_ID.fetch_add(1, Ordering::Relaxed);
    assert!(
        id != usize::MAX,
        "multi-region durable log backend id exhausted"
    );
    id
}

fn thread_owned_region_owner() -> Option<ThreadOwnedRegionOwner> {
    THREAD_OWNED_REGION_OWNER.with(Cell::get)
}

fn set_thread_owned_region_owner(owner: Option<ThreadOwnedRegionOwner>) {
    THREAD_OWNED_REGION_OWNER.with(|slot| slot.set(owner));
}

#[cfg(test)]
struct ScopedThreadOwnedRegionOwner {
    previous: Option<ThreadOwnedRegionOwner>,
}

#[cfg(test)]
impl Drop for ScopedThreadOwnedRegionOwner {
    fn drop(&mut self) {
        set_thread_owned_region_owner(self.previous);
    }
}

#[cfg(test)]
fn scoped_thread_owned_region_owner_for_test(
    backend_id: usize,
    owner: Option<usize>,
) -> ScopedThreadOwnedRegionOwner {
    let previous = thread_owned_region_owner();
    let owner = owner.map(|region_index| ThreadOwnedRegionOwner {
        backend_id,
        region_index,
    });
    set_thread_owned_region_owner(owner);
    ScopedThreadOwnedRegionOwner { previous }
}

#[derive(Debug)]
pub(super) struct MultiRegionDurableLogBackend<R>
where
    R: DurableRegionStorage,
{
    backend_id: usize,
    regions: Vec<DurableRegionLog<R>>,
    selection: RegionSelection,
    placements: Vec<RegionPlacement>,
    #[cfg(test)]
    last_planned_recovery_worker_counts: Mutex<Vec<usize>>,
    #[cfg(test)]
    last_recovery_timing: Mutex<Option<MultiRegionRecoveryTimingForTest>>,
}

impl<R> MultiRegionDurableLogBackend<R>
where
    R: DurableRegionStorage,
{
    fn region_placement(&self, index: usize) -> Option<&RegionPlacement> {
        self.placements.get(index)
    }

    fn thread_owned_region_index(&self) -> Result<usize> {
        if let Some(owner) = thread_owned_region_owner()
            && owner.backend_id == self.backend_id
        {
            let index = owner.region_index;
            ensure!(
                index < self.regions.len(),
                "thread-owned multi-region durable log owner {index} is out of bounds for {} regions",
                self.regions.len()
            );
            return Ok(index);
        }

        ensure!(
            self.regions.iter().enumerate().any(|(index, _)| {
                self.region_placement(index)
                    .and_then(|placement| placement.numa_node)
                    .is_some()
            }),
            "thread-owned multi-region durable log selection requires at least one region placement with a NUMA node"
        );

        let cpu = current_cpu().context(
            "thread-owned multi-region durable log selection requires current CPU support",
        )?;
        let node = node_for_cpu(cpu)
            .with_context(|| {
                format!(
                    "thread-owned multi-region durable log selection requires NUMA node lookup for CPU {cpu}"
                )
            })?
            .with_context(|| {
                format!("thread-owned multi-region durable log selection found no NUMA node for CPU {cpu}")
            })?;

        let Some((index, placement)) = self.regions.iter().enumerate().find_map(|(index, _)| {
            let placement = self.region_placement(index)?;
            (placement.numa_node.map(NumaNode::id) == Some(node)).then_some((index, placement))
        }) else {
            bail!(
                "thread-owned multi-region durable log selection found no region placement for NUMA node {node}"
            );
        };

        let previous_owner = thread_owned_region_owner();
        set_thread_owned_region_owner(Some(ThreadOwnedRegionOwner {
            backend_id: self.backend_id,
            region_index: index,
        }));
        if let Some(cpu_set) = placement.cpu_set.as_ref()
            && let Err(error) = pin_current_thread(cpu_set.as_slice()).with_context(|| {
                format!(
                    "failed to pin thread-owned multi-region durable log thread to region {index}"
                )
            })
        {
            set_thread_owned_region_owner(previous_owner);
            return Err(error);
        }
        Ok(index)
    }

    fn region_recovery_options(&self, _index: usize) -> RecoveryOptions {
        RecoveryOptions {
            parallelism: RecoveryParallelism::Workers(REGION_RECOVERY_WORKERS),
        }
    }

    fn route_virtual_block(&self, data_block: u32) -> Result<Option<RegionBlockRoute>> {
        let mut block_base = 0u32;
        for (region_index, region) in self.regions.iter().enumerate() {
            let block_len = region.mapped_block_len()?;
            let block_end = block_base
                .checked_add(block_len)
                .context("multi-region durable log block range overflow")?;
            if data_block < block_end {
                return Ok(Some(RegionBlockRoute {
                    region_index,
                    region_local_block: data_block - block_base,
                    block_base,
                }));
            }
            block_base = block_end;
        }
        Ok(None)
    }

    fn route_virtual_block_required(
        &self,
        data_block: u32,
        what: &str,
    ) -> Result<RegionBlockRoute> {
        self.route_virtual_block(data_block)?.with_context(|| {
            format!("{what} data block {data_block} is outside all configured region ranges")
        })
    }

    #[cfg(test)]
    fn record_planned_recovery_worker_counts_for_test(&self, worker_counts: Vec<usize>) {
        *self.last_planned_recovery_worker_counts.lock().unwrap() = worker_counts;
    }

    #[cfg(test)]
    fn record_recovery_timing_for_test(&self, timing: MultiRegionRecoveryTimingForTest) {
        *self.last_recovery_timing.lock().unwrap() = Some(timing);
    }

    fn recovered_region_inputs(
        &self,
    ) -> Result<(
        Vec<crate::runtime::vm::RecoveredRegionInput>,
        Vec<std::time::Duration>,
        Vec<Vec<Option<i32>>>,
    )> {
        #[cfg(test)]
        {
            let worker_counts = self
                .regions
                .iter()
                .enumerate()
                .map(
                    |(index, _)| match self.region_recovery_options(index).parallelism {
                        RecoveryParallelism::Serial => 1,
                        RecoveryParallelism::Auto => std::thread::available_parallelism()
                            .map(usize::from)
                            .unwrap_or(1),
                        RecoveryParallelism::Workers(workers) => workers.max(1),
                    },
                )
                .collect::<Vec<_>>();
            self.record_planned_recovery_worker_counts_for_test(worker_counts);
        }

        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(self.regions.len());
            for (index, region) in self.regions.iter().enumerate() {
                let placement = self.region_placement(index).cloned();
                let options = self.region_recovery_options(index);
                handles.push(scope.spawn(move || -> Result<_> {
                    let started = std::time::Instant::now();
                    if let Some(cpu_set) = placement.and_then(|placement| placement.cpu_set) {
                        pin_current_thread(cpu_set.as_slice()).with_context(|| {
                            format!(
                                "failed to pin multi-region durable log recovery thread for region {index}"
                            )
                        })?;
                    }
                    let input = region
                        .recovered_region_input_with_options(options)
                        .with_context(|| {
                            format!("failed to recover multi-region durable log region {index}")
                        })?;
                    #[cfg(test)]
                    let worker_nodes = input.recovered.recovery_worker_nodes_for_test().to_vec();
                    #[cfg(not(test))]
                    let worker_nodes = Vec::new();
                    Ok((input, started.elapsed(), worker_nodes))
                }));
            }

            let mut inputs = Vec::with_capacity(handles.len());
            let mut elapsed = Vec::with_capacity(handles.len());
            let mut worker_nodes = Vec::with_capacity(handles.len());
            for handle in handles {
                match handle.join() {
                    Ok(result) => {
                        let (input, region_elapsed, region_worker_nodes) = result?;
                        inputs.push(input);
                        elapsed.push(region_elapsed);
                        worker_nodes.push(region_worker_nodes);
                    }
                    Err(panic) => std::panic::resume_unwind(panic),
                }
            }
            Ok((inputs, elapsed, worker_nodes))
        })
    }

    fn selected_region_index(&self) -> Result<usize> {
        match self.selection {
            RegionSelection::Pinned(index) => {
                ensure!(
                    index < self.regions.len(),
                    "pinned multi-region durable log selection {index} is out of bounds for {} regions",
                    self.regions.len()
                );
                Ok(index)
            }
            RegionSelection::ThreadOwned => self.thread_owned_region_index(),
        }
    }

    fn selected_region(&self) -> Result<&DurableRegionLog<R>> {
        let index = self.selected_region_index()?;
        self.regions
            .get(index)
            .context("selected multi-region durable log region is missing")
    }

    fn selected_region_mut(&mut self) -> Result<&mut DurableRegionLog<R>> {
        let index = self.selected_region_index()?;
        self.regions
            .get_mut(index)
            .context("selected multi-region durable log region is missing")
    }

    fn merged_recovered_region_snapshot(
        &self,
    ) -> Result<Option<crate::runtime::vm::RecoveredRegion>> {
        if self.regions.is_empty() {
            #[cfg(test)]
            self.record_planned_recovery_worker_counts_for_test(Vec::new());
            return Ok(None);
        }
        let total_started = std::time::Instant::now();
        let (inputs, region_recovery_elapsed, region_worker_nodes) =
            self.recovered_region_inputs()?;
        let merge_started = std::time::Instant::now();
        let recovered = crate::runtime::vm::RecoveredRegion::merge_region_set(inputs)?;
        let merge_elapsed = merge_started.elapsed();
        let total_elapsed = total_started.elapsed();
        #[cfg(test)]
        self.record_recovery_timing_for_test(MultiRegionRecoveryTimingForTest {
            region_recovery_elapsed,
            region_worker_nodes,
            merge_elapsed,
            total_elapsed,
        });
        #[cfg(not(test))]
        {
            let _ = (
                region_recovery_elapsed,
                region_worker_nodes,
                merge_elapsed,
                total_elapsed,
            );
        }
        Ok(Some(recovered))
    }

    #[cfg(test)]
    fn backend_id_for_test(&self) -> usize {
        self.backend_id
    }

    #[cfg(test)]
    fn latest_planned_recovery_worker_counts_for_test(&self) -> Vec<usize> {
        self.last_planned_recovery_worker_counts
            .lock()
            .unwrap()
            .clone()
    }

    #[cfg(test)]
    fn latest_recovery_timing_for_test(&self) -> Option<MultiRegionRecoveryTimingForTest> {
        self.last_recovery_timing.lock().unwrap().clone()
    }

    #[cfg(test)]
    fn latest_planned_recovery_total_workers_for_test(&self) -> usize {
        self.latest_planned_recovery_worker_counts_for_test()
            .into_iter()
            .sum()
    }
}

impl<R> TxDurableLogBackend for MultiRegionDurableLogBackend<R>
where
    R: DurableRegionStorage,
{
    fn ensure_type_layout(&mut self, layout: &PersistentTypeLayout) -> Result<()> {
        for region in &mut self.regions {
            region.ensure_type_layout(layout)?;
        }
        Ok(())
    }

    fn has_type_layout(&self, id: TypeLayoutId) -> Result<bool> {
        for region in &self.regions {
            if !region.has_type_layout(id)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn append_data_record(
        &mut self,
        transaction_stream_id: u32,
        data_stream: DurableDataStream,
        record: &[u8],
    ) -> Result<DurableDataRecordPointer> {
        self.selected_region_mut()?
            .append_data_record(transaction_stream_id, data_stream, record)
    }

    fn append_log_entry(&mut self, transaction_stream_id: u32, entry: TxLogEntry) -> Result<()> {
        self.selected_region_mut()?
            .append_log_entry(transaction_stream_id, entry)
    }

    fn mark_last_log_entry_committed(
        &mut self,
        transaction_stream_id: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        self.selected_region_mut()?
            .mark_last_log_entry_committed(transaction_stream_id, marker)
    }

    fn flush_data(&mut self) -> Result<()> {
        self.selected_region_mut()?.flush_data()
    }

    fn flush_log(&mut self) -> Result<()> {
        self.selected_region_mut()?.flush_log()
    }

    fn fence(&mut self) -> Result<()> {
        self.selected_region_mut()?.fence()
    }

    fn retire_committed_linear_undo_chunk(&mut self, chunk_start_block: u32) -> Result<()> {
        self.selected_region_mut()?
            .retire_committed_linear_undo_chunk(chunk_start_block)
    }

    fn with_recovered_region_snapshot(
        &self,
        f: &mut dyn FnMut(
            &crate::runtime::vm::RecoveredRegion,
            Option<alloc::sync::Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>>,
        ) -> Result<()>,
    ) -> Result<bool> {
        let Some(recovered) = self.merged_recovered_region_snapshot()? else {
            return Ok(false);
        };
        let mapped_source = recovered
            .cloned_mapped_region_source()
            .context("merged recovered region is missing composite mapped source")?;
        f(&recovered, Some(mapped_source))?;
        Ok(true)
    }

    fn recover_region_snapshot(&self) -> Result<Option<crate::runtime::vm::RecoveredRegion>> {
        self.merged_recovered_region_snapshot()
    }

    fn object_data_chunk_start_for_block(&self, data_block: u32) -> Result<Option<u32>> {
        let Some(route) = self.route_virtual_block(data_block)? else {
            return Ok(None);
        };
        let region = &self.regions[route.region_index];
        let Some(chunk_start) =
            region.object_data_chunk_start_for_block(route.region_local_block)?
        else {
            return Ok(None);
        };
        Ok(Some(chunk_start.checked_add(route.block_base).context(
            "multi-region durable log chunk start overflow",
        )?))
    }

    fn cloned_mapped_region_source(
        &self,
    ) -> Option<alloc::sync::Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>> {
        self.selected_region()
            .ok()
            .and_then(DurableRegionLog::cloned_mapped_region_source)
    }

    fn retire_whole_dead_object_chunks(
        &mut self,
        reachable: &[crate::runtime::transaction::PersistentRecoveredRecordLocation],
        unreachable: &[crate::runtime::transaction::PersistentRecoveredRecordLocation],
    ) -> Result<Vec<u32>> {
        let mut reachable_by_region = vec![Vec::new(); self.regions.len()];
        let mut unreachable_by_region = vec![Vec::new(); self.regions.len()];
        let mut block_bases = vec![None; self.regions.len()];

        for &location in reachable {
            let route = self.route_virtual_block_required(
                location.data_block,
                "persistent GC reachable object record",
            )?;
            block_bases[route.region_index].get_or_insert(route.block_base);
            let mut local = location;
            local.data_block = route.region_local_block;
            reachable_by_region[route.region_index].push(local);
        }

        for &location in unreachable {
            let route = self.route_virtual_block_required(
                location.data_block,
                "persistent GC unreachable object record",
            )?;
            block_bases[route.region_index].get_or_insert(route.block_base);
            let mut local = location;
            local.data_block = route.region_local_block;
            unreachable_by_region[route.region_index].push(local);
        }

        let mut retired = Vec::new();
        for (region_index, region) in self.regions.iter_mut().enumerate() {
            let Some(block_base) = block_bases[region_index] else {
                continue;
            };
            let local_retired = region
                .retire_whole_dead_object_chunks(
                    &reachable_by_region[region_index],
                    &unreachable_by_region[region_index],
                )
                .with_context(|| {
                    format!("failed to retire persistent GC chunks in region {region_index}")
                })?;
            for local_chunk_start in local_retired {
                retired.push(
                    local_chunk_start
                        .checked_add(block_base)
                        .context("multi-region durable log retired chunk start overflow")?,
                );
            }
        }

        Ok(retired)
    }

    #[cfg(test)]
    fn log_entries_for_test(&self, stream_id: u32) -> Vec<TxLogEntry> {
        let index = self
            .selected_region_index()
            .expect("multi-region durable log test helper requires a selected region");
        self.log_entries_for_region_for_test(index, stream_id)
    }

    #[cfg(test)]
    fn latest_planned_recovery_worker_counts_for_test(&self) -> Vec<usize> {
        MultiRegionDurableLogBackend::latest_planned_recovery_worker_counts_for_test(self)
    }

    #[cfg(test)]
    fn latest_multi_region_recovery_timing_for_test(
        &self,
    ) -> Option<MultiRegionRecoveryTimingForTest> {
        MultiRegionDurableLogBackend::latest_recovery_timing_for_test(self)
    }

    #[cfg(test)]
    fn region_count_for_test(&self) -> usize {
        self.regions.len()
    }

    #[cfg(test)]
    fn log_entries_for_region_for_test(
        &self,
        region_index: usize,
        stream_id: u32,
    ) -> Vec<TxLogEntry> {
        self.regions
            .get(region_index)
            .map(|region| region.log_entries_for_test(stream_id))
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn has_type_layout_for_region_for_test(
        &self,
        region_index: usize,
        id: TypeLayoutId,
    ) -> Result<bool> {
        match self.regions.get(region_index) {
            Some(region) => region.has_type_layout(id),
            None => Ok(false),
        }
    }

    #[cfg(test)]
    fn data_chunk_stream_id_for_data_block_for_test(&self, data_block: u32) -> Result<u32> {
        self.selected_region()?
            .data_chunk_stream_id_for_data_block_for_test(data_block)
    }
}

#[cfg(test)]
impl MultiRegionDurableLogBackend<crate::runtime::vm::block_region::DaxPmemBlockRegion> {
    pub(super) fn new_for_test(
        region_count: usize,
        payload_blocks: usize,
        selection: RegionSelection,
    ) -> Result<Self> {
        ensure!(
            region_count > 0,
            "test multi-region durable log requires regions"
        );
        let mut regions = Vec::with_capacity(region_count);
        for _ in 0..region_count {
            let region =
                crate::runtime::vm::block_region::DaxPmemBlockRegion::new_for_test(payload_blocks)?;
            regions.push(DurableRegionLog::new(region, None));
        }
        Ok(Self {
            backend_id: next_multi_region_backend_id(),
            placements: vec![RegionPlacement::default(); region_count],
            regions,
            selection,
            #[cfg(test)]
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            #[cfg(test)]
            last_recovery_timing: Mutex::new(None),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
impl MultiRegionDurableLogBackend<crate::runtime::vm::block_region::DaxPmemBlockRegion> {
    pub(super) fn create_fsdax_regions_for_test(regions: Vec<TMemoryRegionConfig>) -> Result<Self> {
        ensure!(
            !regions.is_empty(),
            "test multi-region durable log requires regions"
        );
        let placements = placements_for_regions(&regions)?;
        let mut durable_regions = Vec::with_capacity(regions.len());
        for region in &regions {
            let num_blocks = usize::try_from(fixed_window_num_blocks(region)?)
                .context("multi-region DAX durable log block count overflow")?;
            let reserved_blocks = crate::runtime::vm::tmemory_reserved_metadata_blocks(num_blocks)?;
            ensure!(
                num_blocks >= reserved_blocks,
                "multi-region DAX durable log region {} cannot fit metadata into reserved window",
                region.path.display()
            );
            let payload_blocks = num_blocks - reserved_blocks;
            let backend =
                crate::runtime::vm::block_region::DaxPmemBlockRegion::create_fsdax_path_in_window(
                    region.path.clone(),
                    payload_blocks,
                    fixed_window_for_region(region),
                )?;
            durable_regions.push(DurableRegionLog::new(backend, None));
        }
        Ok(Self {
            backend_id: next_multi_region_backend_id(),
            placements,
            regions: durable_regions,
            selection: RegionSelection::Pinned(0),
            #[cfg(test)]
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            #[cfg(test)]
            last_recovery_timing: Mutex::new(None),
        })
    }

    pub(super) fn open_fsdax_regions_for_test(regions: Vec<TMemoryRegionConfig>) -> Result<Self> {
        ensure!(
            !regions.is_empty(),
            "test multi-region durable log requires regions"
        );
        let placements = placements_for_regions(&regions)?;
        let mut durable_regions = Vec::with_capacity(regions.len());
        for region in &regions {
            let num_blocks = usize::try_from(fixed_window_num_blocks(region)?)
                .context("multi-region DAX durable log block count overflow")?;
            let reserved_blocks = crate::runtime::vm::tmemory_reserved_metadata_blocks(num_blocks)?;
            ensure!(
                num_blocks >= reserved_blocks,
                "multi-region DAX durable log region {} cannot fit metadata into reserved window",
                region.path.display()
            );
            let payload_blocks = num_blocks - reserved_blocks;
            let backend =
                crate::runtime::vm::block_region::DaxPmemBlockRegion::open_fsdax_path_in_window(
                    region.path.clone(),
                    payload_blocks,
                    fixed_window_for_region(region),
                )?;
            durable_regions.push(DurableRegionLog::new(backend, None));
        }
        Ok(Self {
            backend_id: next_multi_region_backend_id(),
            placements,
            regions: durable_regions,
            selection: RegionSelection::Pinned(0),
            #[cfg(test)]
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            #[cfg(test)]
            last_recovery_timing: Mutex::new(None),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
impl MultiRegionDurableLogBackend<crate::runtime::vm::block_region::FileBackedMemoryBlockRegion> {
    pub(super) fn create_file_backed_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        ensure!(
            !regions.is_empty(),
            "test multi-region durable log requires regions"
        );
        let placements = placements_for_regions(&regions)?;
        let mut durable_regions = Vec::with_capacity(regions.len());
        for region in &regions {
            let backend =
                crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::create_in_window(
                    &region.path,
                    fixed_window_num_blocks(region)?,
                    fixed_window_for_region(region),
                )?;
            durable_regions.push(DurableRegionLog::new(backend, None));
        }
        Ok(Self {
            backend_id: next_multi_region_backend_id(),
            placements,
            regions: durable_regions,
            selection: RegionSelection::Pinned(0),
            #[cfg(test)]
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            #[cfg(test)]
            last_recovery_timing: Mutex::new(None),
        })
    }

    pub(super) fn open_file_backed_regions_for_test(
        regions: Vec<TMemoryRegionConfig>,
    ) -> Result<Self> {
        ensure!(
            !regions.is_empty(),
            "test multi-region durable log requires regions"
        );
        let placements = placements_for_regions(&regions)?;
        let mut durable_regions = Vec::with_capacity(regions.len());
        for region in &regions {
            let mut backend =
                crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_in_window(
                    &region.path,
                    fixed_window_for_region(region),
                )?;
            crate::runtime::vm::block_region::retire_completed_linear_undo_chunks(&mut backend)?;
            durable_regions.push(DurableRegionLog::new(backend, None));
        }
        Ok(Self {
            backend_id: next_multi_region_backend_id(),
            placements,
            regions: durable_regions,
            selection: RegionSelection::Pinned(0),
            #[cfg(test)]
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            #[cfg(test)]
            last_recovery_timing: Mutex::new(None),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MultiRegionDurableLogBackend, RegionPlacement, RegionSelection,
        scoped_thread_owned_region_owner_for_test,
    };
    use crate::prelude::*;
    use crate::runtime::transaction::encode_object_record_for_test;
    use crate::runtime::transaction::persist::{
        DurableDataRecordPointer, DurableDataStream, DurableRegionLog, DurableRegionStorage,
        PendingCommitLogEntry, PendingPublication, TxDurableLog, TxDurableLogBackend,
    };
    use crate::runtime::transaction::type_layout::{
        PersistentTypeLayout, TypeLayoutId, TypeLayoutRegistry,
    };
    use crate::runtime::transaction::{
        ObjectPayload, ObjectValue, PersistentRecoveredRecordLocation, StreamPublisher,
    };
    use crate::runtime::vm::block_region::{
        BlockRegionBackendView, StreamCursor, VMemoryBlockRegion,
    };
    use crate::runtime::vm::{PackedGranuleDomain, TMemory, TxLogEntry, TxLogEntryRole};
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingRegionState {
        flush_data_count: u32,
        flush_log_count: u32,
        fence_count: u32,
    }

    #[derive(Debug)]
    struct RecordingRegion {
        inner: VMemoryBlockRegion,
        state: Arc<Mutex<RecordingRegionState>>,
        fail_flush_data: bool,
        fail_flush_log: bool,
        fail_fence: bool,
        next_data_block: u32,
        next_log_block: u32,
    }

    impl RecordingRegion {
        fn new(
            state: Arc<Mutex<RecordingRegionState>>,
            fail_flush_data: bool,
            fail_flush_log: bool,
            fail_fence: bool,
        ) -> Self {
            Self {
                inner: VMemoryBlockRegion::new_for_test(32).unwrap(),
                state,
                fail_flush_data,
                fail_flush_log,
                fail_fence,
                next_data_block: 1,
                next_log_block: 1,
            }
        }
    }

    impl DurableRegionStorage for RecordingRegion {
        fn stream_cursor(&mut self, stream_id: u32) -> StreamCursor {
            self.inner.stream_cursor(stream_id)
        }

        fn refresh_from_image(&mut self) -> Result<()> {
            Ok(())
        }

        fn append_type_layout_metadata(&mut self, _layout: &PersistentTypeLayout) -> Result<()> {
            Ok(())
        }

        fn load_type_layout_metadata(&self) -> Result<TypeLayoutRegistry> {
            Ok(TypeLayoutRegistry::default())
        }

        fn append_data_record(
            &mut self,
            _stream: StreamCursor,
            _record: &[u8],
        ) -> Result<DurableDataRecordPointer> {
            let data_block = self.next_data_block;
            self.next_data_block += 1;
            Ok(DurableDataRecordPointer {
                chunk_start_block: data_block,
                data_block,
                data_offset: 0,
                data_block_generation: 0,
            })
        }

        fn append_data_record_requires_allocation(
            &self,
            _stream: StreamCursor,
            _record: &[u8],
        ) -> Result<bool> {
            Ok(false)
        }

        fn append_log_entry(&mut self, _stream: StreamCursor, _entry: TxLogEntry) -> Result<u32> {
            let log_block = self.next_log_block;
            self.next_log_block += 1;
            Ok(log_block)
        }

        fn append_log_entry_requires_allocation(&self, _stream: StreamCursor) -> Result<bool> {
            Ok(false)
        }

        fn mark_last_log_entry_committed(
            &mut self,
            _stream: StreamCursor,
            _marker: PendingCommitLogEntry,
        ) -> Result<u32> {
            Ok(self.next_log_block.saturating_sub(1))
        }

        fn flush_data_chunk(&self, _chunk_start_block: u32) -> Result<()> {
            self.state.lock().unwrap().flush_data_count += 1;
            if self.fail_flush_data {
                bail!("unselected region data flush failure");
            }
            Ok(())
        }

        fn flush_log_block(&self, _log_block: u32) -> Result<()> {
            self.state.lock().unwrap().flush_log_count += 1;
            if self.fail_flush_log {
                bail!("unselected region log flush failure");
            }
            Ok(())
        }

        fn fence(&self) -> Result<()> {
            self.state.lock().unwrap().fence_count += 1;
            if self.fail_fence {
                bail!("unselected region fence failure");
            }
            Ok(())
        }

        fn retire_linear_undo_chunks<I>(&mut self, _chunk_starts: I) -> Result<Vec<u32>>
        where
            I: IntoIterator<Item = u32>,
        {
            Ok(Vec::new())
        }

        fn recover_region_snapshot(&self) -> Result<crate::runtime::vm::RecoveredRegion> {
            unimplemented!("recording region recovery is not used by these tests")
        }

        fn object_data_chunk_start_for_block(&self, _data_block: u32) -> Result<Option<u32>> {
            Ok(None)
        }

        fn retire_whole_dead_object_chunks(
            &mut self,
            _reachable: &[crate::runtime::transaction::PersistentRecoveredRecordLocation],
            _unreachable: &[crate::runtime::transaction::PersistentRecoveredRecordLocation],
        ) -> Result<Vec<u32>> {
            Ok(Vec::new())
        }

        fn view(&self) -> BlockRegionBackendView<'_> {
            self.inner.view()
        }
    }

    fn test_layout() -> PersistentTypeLayout {
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::new(112).unwrap(),
            fingerprint: 0x0112_0000_0000_0004,
            body_size: 4,
            fields: vec![],
        }
    }

    fn test_publication(type_layout_id: u32) -> PendingPublication {
        test_publication_with(41, 7, type_layout_id, 9)
    }

    fn test_publication_with(
        logical_id: u64,
        version: u32,
        type_layout_id: u32,
        value: i32,
    ) -> PendingPublication {
        PendingPublication::persistent_object(
            PackedGranuleDomain::TStruct,
            logical_id,
            version,
            type_layout_id,
            encode_object_record_for_test(
                logical_id,
                version,
                type_layout_id,
                &ObjectPayload::Struct(vec![ObjectValue::I32(value)]),
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn multi_region_durable_log_routes_transaction_to_owned_region() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(1)).unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        let layout = test_layout();
        log.ensure_type_layout(&layout).unwrap();
        let publication = test_publication(layout.id().get());

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        assert!(log.log_entries_for_region_for_test(0, 12).is_empty());
        let entries = log.log_entries_for_region_for_test(1, 12);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TObjectPub);
        assert_eq!(entries[0].tx_meta & 1, 1);
    }

    #[test]
    fn multi_region_durable_log_exposes_region_count_for_tests() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(1)).unwrap();
        let log = TxDurableLog::with_backend(backend);

        assert_eq!(log.region_count_for_test(), 2);
    }

    #[test]
    fn multi_region_durable_log_fans_out_type_layout_metadata_to_all_regions() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(1)).unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        let layout = test_layout();

        log.ensure_type_layout(&layout).unwrap();

        assert_eq!(log.region_count_for_test(), 2);
        assert!(log.has_type_layout(layout.id()).unwrap());
        assert!(
            log.has_type_layout_for_region_for_test(0, layout.id())
                .unwrap()
        );
        assert!(
            log.has_type_layout_for_region_for_test(1, layout.id())
                .unwrap()
        );
    }

    #[test]
    fn multi_region_durable_log_rejects_pinned_region_out_of_bounds() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(2)).unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        let publication = test_publication(test_layout().id().get());
        let err = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap_err()
        };

        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn multi_region_durable_log_rejects_thread_owned_selection_without_owner_or_placement() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::ThreadOwned)
                .unwrap();
        let _owner = scoped_thread_owned_region_owner_for_test(backend.backend_id_for_test(), None);
        let mut log = TxDurableLog::with_backend(backend);
        let publication = test_publication(test_layout().id().get());
        let err = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap_err()
        };

        assert!(
            err.to_string()
                .contains("requires at least one region placement")
        );
    }

    #[test]
    fn multi_region_durable_log_routes_thread_owned_transaction_to_scoped_region_owner() {
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::ThreadOwned)
                .unwrap();
        let _owner =
            scoped_thread_owned_region_owner_for_test(backend.backend_id_for_test(), Some(1));
        let mut log = TxDurableLog::with_backend(backend);
        let layout = test_layout();
        log.ensure_type_layout(&layout).unwrap();
        let publication = test_publication(layout.id().get());

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        assert!(log.log_entries_for_region_for_test(0, 12).is_empty());
        let entries = log.log_entries_for_region_for_test(1, 12);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].role().unwrap(), TxLogEntryRole::TObjectPub);
        assert_eq!(entries[0].tx_meta & 1, 1);
    }

    #[test]
    fn multi_region_durable_log_ignores_thread_owned_owner_from_other_backend() {
        let stale_backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::ThreadOwned)
                .unwrap();
        let _owner =
            scoped_thread_owned_region_owner_for_test(stale_backend.backend_id_for_test(), Some(1));
        let backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::ThreadOwned)
                .unwrap();
        let mut log = TxDurableLog::with_backend(backend);
        let publication = test_publication(test_layout().id().get());
        let err = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap_err()
        };

        assert!(
            err.to_string()
                .contains("requires at least one region placement")
        );
    }

    #[test]
    fn multi_region_durable_log_does_not_flush_or_fence_unselected_regions_on_commit_path() {
        let selected_state = Arc::new(Mutex::new(RecordingRegionState::default()));
        let unselected_state = Arc::new(Mutex::new(RecordingRegionState::default()));
        let backend = MultiRegionDurableLogBackend {
            backend_id: super::next_multi_region_backend_id(),
            placements: vec![RegionPlacement::default(); 2],
            regions: vec![
                DurableRegionLog::new(
                    RecordingRegion::new(selected_state.clone(), false, false, false),
                    None,
                ),
                DurableRegionLog::new(
                    RecordingRegion::new(unselected_state.clone(), true, true, true),
                    None,
                ),
            ],
            selection: RegionSelection::Pinned(0),
            last_planned_recovery_worker_counts: Mutex::new(Vec::new()),
            last_recovery_timing: Mutex::new(None),
        };
        let mut log = TxDurableLog::with_backend(backend);
        let publication = test_publication(test_layout().id().get());

        let marker = {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher
                .publish_object_publication_before_commit(&publication)
                .unwrap()
        };
        {
            let mut sink = log.stream_sink(12);
            let mut publisher = StreamPublisher::new(&mut sink, 12, 12);
            publisher.publish_commit_lp(marker).unwrap();
        }

        let selected_state = selected_state.lock().unwrap();
        assert!(selected_state.flush_data_count > 0);
        assert!(selected_state.flush_log_count > 0);
        assert!(selected_state.fence_count > 0);

        let unselected_state = unselected_state.lock().unwrap();
        assert_eq!(unselected_state.flush_data_count, 0);
        assert_eq!(unselected_state.flush_log_count, 0);
        assert_eq!(unselected_state.fence_count, 0);
    }

    #[test]
    fn multi_region_durable_log_object_data_chunk_start_for_second_region_includes_block_base() {
        let mut backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0)).unwrap();
        let publication = test_publication(test_layout().id().get());
        let record = super::super::persist::encode_data_record(&publication).unwrap();
        let pointer = TxDurableLogBackend::append_data_record(
            &mut backend.regions[1],
            12,
            DurableDataStream::ObjectPublication,
            &record,
        )
        .unwrap();
        let block_base = backend.regions[0].mapped_block_len().unwrap();

        assert_eq!(
            TxDurableLogBackend::object_data_chunk_start_for_block(
                &backend,
                block_base + pointer.data_block,
            )
            .unwrap(),
            Some(block_base + pointer.chunk_start_block)
        );
    }

    #[test]
    fn multi_region_gc_retires_chunks_in_owning_region() {
        let layout = test_layout();
        let publication_region0 = test_publication_with(41, 7, layout.id().get(), 11);
        let publication_region1 = test_publication_with(99, 3, layout.id().get(), 22);

        let mut backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0)).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[0], &layout).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[1], &layout).unwrap();
        append_committed_publication_to_region_log(
            &mut backend.regions[0],
            12,
            &publication_region0,
        )
        .unwrap();
        let region1_pointer = append_committed_publication_to_region_log(
            &mut backend.regions[1],
            24,
            &publication_region1,
        )
        .unwrap();
        let region1_block_base = backend.regions[0].mapped_block_len().unwrap();

        let recovered = TxDurableLogBackend::recover_region_snapshot(&backend)
            .unwrap()
            .unwrap();
        let winners = recovered.committed_object_winners().unwrap();
        let reachable = winners
            .iter()
            .find(|winner| winner.object_id == 41)
            .map(|winner| PersistentRecoveredRecordLocation {
                object_id: crate::runtime::transaction::ObjectId { object_index: 41 },
                version: winner.version,
                data_block: winner.data_block,
                data_offset: winner.data_offset,
                record_len: winner.record_len,
            })
            .into_iter()
            .collect::<Vec<_>>();
        let unreachable = winners
            .iter()
            .find(|winner| winner.object_id == 99)
            .map(|winner| PersistentRecoveredRecordLocation {
                object_id: crate::runtime::transaction::ObjectId { object_index: 99 },
                version: winner.version,
                data_block: winner.data_block,
                data_offset: winner.data_offset,
                record_len: winner.record_len,
            })
            .into_iter()
            .collect::<Vec<_>>();

        assert_eq!(reachable.len(), 1);
        assert_eq!(unreachable.len(), 1);
        assert!(unreachable[0].data_block >= region1_block_base);

        let retired = TxDurableLogBackend::retire_whole_dead_object_chunks(
            &mut backend,
            &reachable,
            &unreachable,
        )
        .unwrap();

        assert_eq!(
            retired,
            vec![region1_block_base + region1_pointer.chunk_start_block]
        );
    }

    #[test]
    fn multi_region_gc_rejects_out_of_range_virtual_block() {
        let mut backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0)).unwrap();
        let total_blocks = backend
            .regions
            .iter()
            .map(|region| region.mapped_block_len().unwrap())
            .sum::<u32>();

        let err = TxDurableLogBackend::retire_whole_dead_object_chunks(
            &mut backend,
            &[],
            &[PersistentRecoveredRecordLocation {
                object_id: crate::runtime::transaction::ObjectId { object_index: 7 },
                version: 1,
                data_block: total_blocks,
                data_offset: 0,
                record_len: 1,
            }],
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("outside all configured region ranges"),
            "{err:#}"
        );
    }

    #[test]
    fn multi_region_durable_log_recovery_merges_region_snapshots_and_mapped_source() {
        let layout = test_layout();
        let expected_record = encode_object_record_for_test(
            41,
            8,
            layout.id().get(),
            &ObjectPayload::Struct(vec![ObjectValue::I32(22)]),
        )
        .unwrap();
        let publication_v1 = PendingPublication::persistent_object(
            PackedGranuleDomain::TStruct,
            41,
            7,
            layout.id().get(),
            encode_object_record_for_test(
                41,
                7,
                layout.id().get(),
                &ObjectPayload::Struct(vec![ObjectValue::I32(11)]),
            )
            .unwrap(),
        )
        .unwrap();
        let publication_v2 = PendingPublication::persistent_object(
            PackedGranuleDomain::TStruct,
            41,
            8,
            layout.id().get(),
            expected_record.clone(),
        )
        .unwrap();

        let mut backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0)).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[0], &layout).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[1], &layout).unwrap();
        append_committed_publication_to_region_log(&mut backend.regions[0], 12, &publication_v1)
            .unwrap();
        append_committed_publication_to_region_log(&mut backend.regions[1], 12, &publication_v2)
            .unwrap();

        let recovered = TxDurableLogBackend::recover_region_snapshot(&backend)
            .unwrap()
            .unwrap();
        let winners = recovered.committed_object_winners().unwrap();

        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].object_id, 41);
        assert_eq!(winners[0].version, 8);

        let mapped_source = recovered.cloned_mapped_region_source().unwrap();
        let mut observed = Vec::new();
        winners[0]
            .with_source_record_bytes(mapped_source.as_ref(), &mut |bytes| {
                observed.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(observed, expected_record);

        let mut callback_seen = false;
        TxDurableLogBackend::with_recovered_region_snapshot(&backend, &mut |recovered, source| {
            let source = source.expect("merged region recovery should provide a mapped source");
            let winner = recovered.committed_object_winners()?.remove(0);
            let mut callback_record = Vec::new();
            winner.with_source_record_bytes(source.as_ref(), &mut |bytes| {
                callback_record.extend_from_slice(bytes);
                Ok(())
            })?;
            assert_eq!(callback_record, expected_record);
            callback_seen = true;
            Ok(())
        })
        .unwrap();
        assert!(callback_seen);
    }

    #[test]
    fn multi_region_parallel_recovery_recovers_all_regions_and_plans_8_workers_per_region() {
        let layout = test_layout();
        let publication_region0 = test_publication_with(41, 7, layout.id().get(), 11);
        let publication_region1 = test_publication_with(99, 3, layout.id().get(), 22);

        let mut backend =
            MultiRegionDurableLogBackend::new_for_test(2, 32, RegionSelection::Pinned(0)).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[0], &layout).unwrap();
        TxDurableLogBackend::ensure_type_layout(&mut backend.regions[1], &layout).unwrap();
        append_committed_publication_to_region_log(
            &mut backend.regions[0],
            12,
            &publication_region0,
        )
        .unwrap();
        append_committed_publication_to_region_log(
            &mut backend.regions[1],
            24,
            &publication_region1,
        )
        .unwrap();

        let recovered = TxDurableLogBackend::recover_region_snapshot(&backend)
            .unwrap()
            .unwrap();
        let mut winners = recovered.committed_object_winners().unwrap();
        winners.sort_by_key(|winner| winner.object_id);

        assert_eq!(winners.len(), 2);
        assert_eq!(winners[0].object_id, 41);
        assert_eq!(winners[0].version, 7);
        assert_eq!(winners[1].object_id, 99);
        assert_eq!(winners[1].version, 3);
        assert_eq!(
            backend.latest_planned_recovery_worker_counts_for_test(),
            vec![8, 8],
            "multi-region recovery records the planned/requested worker budget per region"
        );
        assert_eq!(
            backend.latest_planned_recovery_total_workers_for_test(),
            16,
            "multi-region recovery records the total planned/requested worker budget"
        );
        let timing = backend
            .latest_recovery_timing_for_test()
            .expect("multi-region recovery should record phase timing");
        assert_eq!(timing.region_recovery_elapsed.len(), 2);
        assert_eq!(timing.region_worker_nodes.len(), 2);
        assert!(timing.merge_elapsed <= timing.total_elapsed);
    }

    fn append_committed_publication_to_region_log<R>(
        region: &mut DurableRegionLog<R>,
        stream_id: u32,
        publication: &PendingPublication,
    ) -> Result<DurableDataRecordPointer>
    where
        R: DurableRegionStorage,
    {
        let record = super::super::persist::encode_data_record(publication)?;
        let pointer = TxDurableLogBackend::append_data_record(
            region,
            stream_id,
            DurableDataStream::ObjectPublication,
            &record,
        )?;
        let entry = TMemory::publication_log_entry(
            publication.logical_id,
            publication.version,
            stream_id << 1,
            pointer.data_block,
            pointer.data_offset,
            pointer.data_block_generation,
            true,
        )?;
        TxDurableLogBackend::append_log_entry(region, stream_id, entry)?;
        Ok(pointer)
    }
}
