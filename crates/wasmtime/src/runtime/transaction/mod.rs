#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::store::InstanceId;
use crate::runtime::vm::{PackedGranuleDomain, TMemory};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::mem;
use std::path::Path;

mod concurrency;
mod config;
mod durable_ref;
mod granule;
mod ids;
mod object_gc;
mod object_heap;
mod object_table;
mod object_value;
mod persist;
mod promotion;
mod region_runtime;
mod state;
pub(crate) mod type_layout;
mod wasmtime_layout;
use concurrency::ConcurrencyControlState;
#[cfg(all(
    test,
    any(
        feature = "transaction-cc-optimistic-validation",
        feature = "transaction-cc-wound-wait"
    )
))]
use concurrency::TransactionConcurrencyControl;
#[cfg(all(test, feature = "transaction-cc-lockbased"))]
use concurrency::{LockBased, LockBasedConflictKindForTest, LockBasedSnapshotForTest};
#[cfg(all(test, feature = "transaction-cc-nowait-abort"))]
use concurrency::{NoWaitAbort, NoWaitAbortConflictKindForTest};
#[cfg(all(test, feature = "transaction-cc-optimistic-validation"))]
use concurrency::{OptimisticValidation, OptimisticValidationConflictKindForTest};
#[cfg(all(test, feature = "transaction-cc-strict-2pl"))]
use concurrency::{StrictTwoPhaseLocking, StrictTwoPhaseLockingConflictKindForTest};
#[cfg(all(test, feature = "transaction-cc-timestamp-ordering"))]
use concurrency::{TimestampOrdering, TimestampOrderingConflictKindForTest};
#[cfg(all(test, feature = "transaction-cc-wait-die"))]
use concurrency::{WaitDie, WaitDieConflictKindForTest};
#[cfg(all(test, feature = "transaction-cc-wound-wait"))]
use concurrency::{WoundWait, WoundWaitConflictKindForTest};
use config::ConcurrencyControl;
#[cfg(test)]
use config::{ConflictPolicy, DurabilityPolicy, ObjectIndexPersistencePolicy};
pub(crate) use config::{
    TMemoryBackend, TMemoryDaxPmemBacking, TMemoryFileBacking, TMemoryPersistenceMode,
    TransactionConfig,
};
pub(crate) use granule::{
    GranuleId, TMEMORY_GRANULE_SHIFT, TMEMORY_GRANULE_SIZE, TMemoryAccessSnapshot,
    TMemoryGranuleSnapshot,
};
use granule::{PendingMemoryStore, TTABLE_GRANULE_SHIFT, TableElementKey};
pub(crate) use ids::{ObjectId, TransactionId};
#[cfg(test)]
use ids::{
    clear_current_thread_transaction_for_test, clear_current_thread_transaction_on_drop_for_test,
    current_thread_transaction_for_test,
};
use ids::{current_thread_transaction, replace_current_thread_transaction};
#[cfg(test)]
use object_gc::PersistentObjectEdge;
pub(crate) use object_gc::{
    DanglingObjectRef, DanglingObjectRefKind, PersistentGcBudget, PersistentGcCommitDelta,
    PersistentGcState, PersistentGcStepReport, PersistentObjectMarker,
    PersistentRecoveredRecordLocation, PersistentRecoveryGcReport, PersistentRootError,
    PersistentRootErrorKind,
};
#[cfg(test)]
use object_value::OBJECT_VALUE_ABI_TAG_I31;
use object_value::VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED;
#[cfg(test)]
use object_value::{FIRST_TRANSACTION_OBJECT_REF_HANDLE, object_kind_from_u16};
pub(crate) use object_value::{
    I31Value, OBJECT_VALUE_ABI_LIVE_REF_KIND_EXTERN, OBJECT_VALUE_ABI_LIVE_REF_KIND_FUNC,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_GC, OBJECT_VALUE_ABI_LIVE_REF_KIND_I31,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT, OBJECT_VALUE_ABI_LIVE_REF_KIND_UNTYPED,
    OBJECT_VALUE_ABI_TAG_F32, OBJECT_VALUE_ABI_TAG_F64, OBJECT_VALUE_ABI_TAG_I32,
    OBJECT_VALUE_ABI_TAG_I64, OBJECT_VALUE_ABI_TAG_REF, OBJECT_VALUE_ABI_TAG_V128, ObjectKind,
    ObjectPayload, ObjectValue, ObjectValueAbi, PersistentObjectRefRaw, TransactionObjectRefRaw,
};
pub(crate) use wasmtime_layout::{
    PERSISTENT_OBJECT_ABI_SLOT_SIZE, WasmtimePersistentFieldLayout,
    WasmtimePersistentFieldLayoutAbi,
};
#[cfg(test)]
use wasmtime_layout::{
    persistent_layout_for_wasmtime_array_type, persistent_layout_for_wasmtime_struct_type,
    wasmtime_array_layout_fingerprint, wasmtime_struct_layout_fingerprint,
};
pub(crate) type PersistentObjectMarkReport = object_gc::PersistentObjectMarkReport;
pub(crate) type PersistentObjectCompactionReport = object_gc::PersistentObjectCompactionReport;
pub(crate) type PersistentMarkSweepReport = object_gc::PersistentMarkSweepReport;
pub(crate) type PersistentVolatileSweepReport = object_gc::PersistentVolatileSweepReport;
#[cfg(feature = "gc")]
pub(crate) use durable_ref::DurableExternRefHostData;
pub(crate) use durable_ref::{
    DurableExternIdentity, DurableFuncIdentity, DurableReferenceRegistry,
};
pub(crate) use object_heap::TxObjectHeader;
pub(crate) use object_heap::encode_object_record as encode_object_record_for_recovery;
#[cfg(test)]
pub(crate) use object_heap::encode_object_record_for_test;
pub(crate) use object_table::ObjectTable;
use object_table::recovered_record_location_from_winner;
pub use object_table::retire_unreachable_object_chunks_for_test;
pub(crate) use persist::{
    PendingCommitLogEntry, PendingGranuleUndo, StreamPublisher, TxDurableLog,
};
pub(crate) use promotion::{
    OrdinaryGcPromotionAdapter, OrdinaryGcPromotionSource, OrdinaryGcPromotionValue,
};
pub(crate) use region_runtime::TransactionRegionRuntime;
use type_layout::TypeLayoutId;
#[cfg(test)]
use type_layout::{PersistentTypeKind, PersistentTypeLayout, TraceSlotKind, TypeLayoutRegistry};

use state::PersistentRootKey;
pub(crate) use state::{GlobalSnapshot, StagedRecord, TableElementSnapshot, TransactionState};

pub(crate) fn collect_tmemory_access_snapshot(
    tmemory: &TMemory,
    addr: u64,
    len: usize,
) -> Result<TMemoryAccessSnapshot> {
    let range = checked_tmemory_range(addr, len, tmemory.byte_len())?;
    let bytes = tmemory.read_committed(range.clone())?;
    let mut granules = Vec::new();
    let mut current = range.start;

    while current < range.end {
        let granule_index = current / TMEMORY_GRANULE_SIZE;
        let granule_range = tmemory_granule_backing_range(granule_index, tmemory.byte_len())?;
        granules.push(TMemoryGranuleSnapshot {
            granule_index,
            range: granule_range.clone(),
            version: tmemory.granule_version(granule_index)?,
            bytes: tmemory.read_committed(granule_range)?,
        });
        current = range.end.min(granules.last().unwrap().range.end);
    }

    Ok(TMemoryAccessSnapshot {
        base: addr,
        bytes,
        byte_len: tmemory.byte_len(),
        granules,
    })
}

fn checked_tmemory_range(
    addr: u64,
    len: usize,
    backing_len: usize,
) -> Result<core::ops::Range<usize>> {
    let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
    let end = start.checked_add(len).context("tmemory address overflow")?;
    ensure!(
        end <= backing_len,
        "out of bounds tmemory access: range {start}..{end} exceeds backing length {backing_len}"
    );
    Ok(start..end)
}

fn granule_instance(owner_instance: Option<InstanceId>) -> Option<u32> {
    owner_instance.map(InstanceId::as_u32)
}

fn granule_owner_instance(instance: Option<u32>) -> Option<InstanceId> {
    instance.map(InstanceId::from_u32)
}

fn global_granule_id(owner_instance: Option<InstanceId>, global_index: u32) -> GranuleId {
    GranuleId::TGlobal {
        instance: granule_instance(owner_instance),
        global_index,
    }
}

fn object_granule_object_id(granule: GranuleId) -> Option<ObjectId> {
    match granule {
        GranuleId::Object { object_id } => Some(object_id),
        GranuleId::TMemory { .. }
        | GranuleId::TMemorySize { .. }
        | GranuleId::TGlobal { .. }
        | GranuleId::TTable { .. }
        | GranuleId::TTableSize { .. } => None,
    }
}

fn granule_uses_transaction_state_version(granule: GranuleId) -> bool {
    matches!(
        granule,
        GranuleId::TMemorySize { .. }
            | GranuleId::TGlobal { .. }
            | GranuleId::TTable { .. }
            | GranuleId::TTableSize { .. }
    )
}

fn checked_array_range_end(start: usize, len: usize) -> Result<usize> {
    start.checked_add(len).context("out of bounds array access")
}

fn memory_size_granule_id(owner_instance: Option<InstanceId>, memory_index: u32) -> GranuleId {
    GranuleId::TMemorySize {
        instance: granule_instance(owner_instance),
        memory_index,
    }
}

fn table_granule_id(
    owner_instance: Option<InstanceId>,
    table_index: u32,
    element_index: u64,
) -> GranuleId {
    table_granule_id_from_u64(
        owner_instance,
        table_index,
        element_index >> TTABLE_GRANULE_SHIFT,
    )
}

fn table_granule_id_from_u64(
    owner_instance: Option<InstanceId>,
    table_index: u32,
    granule_index: u64,
) -> GranuleId {
    GranuleId::TTable {
        instance: granule_instance(owner_instance),
        table_index,
        granule_index,
    }
}

fn table_element_key(
    owner_instance: Option<InstanceId>,
    table_index: u32,
    element_index: u64,
) -> TableElementKey {
    TableElementKey {
        instance: granule_instance(owner_instance),
        table_index,
        element_index,
    }
}

fn pack_persistent_global_root_index(instance: Option<u32>, global_index: u32) -> Result<u64> {
    Ok((persistent_root_instance_code(instance)? << 32) | u64::from(global_index))
}

fn unpack_persistent_global_root_index(root_index: u64) -> Result<PersistentRootKey> {
    ensure!(
        root_index < (1u64 << 52),
        "persistent global root index does not fit in packed coordinate payload"
    );
    Ok(PersistentRootKey::Global {
        instance: persistent_root_instance_from_code(root_index >> 32)?,
        global_index: root_index as u32,
    })
}

fn pack_persistent_table_root_index(key: TableElementKey) -> Result<u64> {
    // Mirror the packed tmemory logical-id layout so table root records stay
    // unique across instance/table/element coordinates within the 60-bit
    // payload budget.
    let instance_code = persistent_root_instance_code(key.instance)?;
    ensure!(
        key.table_index < (1u32 << 12),
        "persistent table root table index does not fit in packed granule id payload"
    );
    ensure!(
        key.element_index < (1u64 << 28),
        "persistent table root element index does not fit in packed granule id payload"
    );
    Ok((instance_code << 40) | (u64::from(key.table_index) << 28) | key.element_index)
}

fn unpack_persistent_table_root_index(root_index: u64) -> Result<PersistentRootKey> {
    ensure!(
        root_index < (1u64 << 60),
        "persistent table root index does not fit in packed coordinate payload"
    );
    Ok(PersistentRootKey::TableElement(TableElementKey {
        instance: persistent_root_instance_from_code(root_index >> 40)?,
        table_index: ((root_index >> 28) & ((1u64 << 12) - 1)) as u32,
        element_index: root_index & ((1u64 << 28) - 1),
    }))
}

fn persistent_root_instance_code(instance: Option<u32>) -> Result<u64> {
    let code = match instance {
        Some(instance) => u64::from(instance)
            .checked_add(1)
            .context("persistent root instance id overflow")?,
        None => 0,
    };
    ensure!(
        code < (1u64 << 20),
        "persistent root instance id does not fit in packed granule id payload"
    );
    Ok(code)
}

fn persistent_root_instance_from_code(code: u64) -> Result<Option<u32>> {
    ensure!(
        code < (1u64 << 20),
        "persistent root instance id does not fit in packed granule id payload"
    );
    if code == 0 {
        Ok(None)
    } else {
        Ok(Some(
            u32::try_from(code - 1).context("persistent root instance id overflow")?,
        ))
    }
}

fn persistent_root_key_from_logical_id(logical_id: u64) -> Result<Option<PersistentRootKey>> {
    let domain = match logical_id >> 60 {
        value if value == PackedGranuleDomain::TGlobal as u64 => PackedGranuleDomain::TGlobal,
        value if value == PackedGranuleDomain::TTable as u64 => PackedGranuleDomain::TTable,
        _ => return Ok(None),
    };
    let root_index = logical_id & ((1u64 << 60) - 1);
    match domain {
        PackedGranuleDomain::TGlobal => Ok(Some(unpack_persistent_global_root_index(root_index)?)),
        PackedGranuleDomain::TTable => Ok(Some(unpack_persistent_table_root_index(root_index)?)),
        _ => Ok(None),
    }
}

fn table_size_granule_id(owner_instance: Option<InstanceId>, table_index: u32) -> GranuleId {
    GranuleId::TTableSize {
        instance: granule_instance(owner_instance),
        table_index,
    }
}

fn memory_granule_id_from_u64(
    owner_instance: Option<InstanceId>,
    memory_index: u32,
    granule_index: u64,
) -> GranuleId {
    GranuleId::TMemory {
        instance: granule_instance(owner_instance),
        memory_index,
        granule_index,
    }
}

fn memory_granule_id_for_instance(
    instance: Option<u32>,
    memory_index: u32,
    granule_index: usize,
) -> Result<GranuleId> {
    Ok(GranuleId::TMemory {
        instance,
        memory_index,
        granule_index: u64::try_from(granule_index)
            .context("tmemory granule index does not fit u64")?,
    })
}

fn memory_granule_key(
    owner_instance: Option<InstanceId>,
    memory_index: u32,
    granule_index: usize,
) -> Result<GranuleId> {
    memory_granule_id_for_instance(
        granule_instance(owner_instance),
        memory_index,
        granule_index,
    )
}

fn tmemory_granule_backing_range(
    granule_index: usize,
    backing_len: usize,
) -> Result<core::ops::Range<usize>> {
    let start = granule_index
        .checked_mul(TMEMORY_GRANULE_SIZE)
        .context("tmemory granule byte offset overflow")?;
    let end = start.saturating_add(TMEMORY_GRANULE_SIZE).min(backing_len);
    Ok(start..end)
}

pub(crate) fn execute_research_transaction_fixture(
    state: &mut TransactionState,
    operators: &[wasmtime_environ::ResearchTransactionModuleOperator],
) -> Result<()> {
    // SHISOFT-TWASM-MOCK: research fixture executor for metadata-only tests.
    // It handles only `ttry` and `tfail`; executable transaction operators
    // should run through parser/lowering/libcalls instead of this bridge.
    for operator in operators {
        match operator.operator {
            wasmtime_environ::TransactionOperator::TTry => {
                state.begin()?;
            }
            wasmtime_environ::TransactionOperator::TTryEnd => {
                if state.active_transaction().is_some() {
                    state.commit()?;
                } else {
                    state.clear_structured_failure();
                }
            }
            wasmtime_environ::TransactionOperator::TFail => {
                state.fail_structured()?;
            }
            other => {
                bail!(
                    "research transaction fixture executor does not handle {other:?} at function {} offset {}",
                    operator.function_index,
                    operator.body_offset
                );
            }
        }
    }
    if state.active_transaction().is_some() {
        state.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
