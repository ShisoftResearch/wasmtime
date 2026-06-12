#![allow(dead_code)]

use crate::prelude::*;
use crate::runtime::store::InstanceId;
use crate::runtime::vm::TMemory;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::{cell::Cell, mem, ops::Range};
use std::path::PathBuf;

#[path = "transaction/object_heap.rs"]
mod object_heap;
#[path = "transaction/persist.rs"]
mod persist;
pub(crate) use object_heap::TxObjectHeader;
pub(crate) use object_heap::encode_object_record as encode_object_record_for_recovery;
#[cfg(test)]
pub(crate) use object_heap::encode_object_record_for_test;
pub(crate) use persist::{
    PendingCommitLogEntry, PendingGranuleUndo, StreamPublisher, TxDurableLog,
};

// Milestone runtime core for proposal WAST progress. The current runtime uses
// store-local transaction state, `VMemory` and configurable `NVMemory`
// transactional memory storage, and real `tmemory` sidecars. Remaining
// `SHISOFT-TWASM-MOCK` tags in this file identify policy selection and
// object-table gaps.

/// Storage backend selected for transactional memories.
///
/// SHISOFT-TWASM-MOCK: selectable backend shape is present and
/// `TransactionConfig` accepts the implemented in-tree backends.
///
/// Milestone runtime support currently implements `VMemory` and research
/// `NVMemory` by default, with hardware-PMEM mode selectable for experiments.
/// `FileBackedMemory` remains represented so transaction configuration keeps
/// its later persistence shape without changing ordinary Wasmtime memories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryBackend {
    VMemory,
    FileBackedMemory,
    NVMemory,
}

/// Persistence behavior for `NVMemory` block regions.
///
/// The research mode is the default so normal tests do not require PMEM
/// hardware. `RequireHardwarePmem` routes NVMemory through the CLWB/SFENCE
/// persistence engine and fails construction when the host cannot provide it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryPersistenceMode {
    ResearchPretendPmem,
    RequireHardwarePmem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TMemoryFileBacking {
    Temp,
    Path(PathBuf),
}

/// SHISOFT-TWASM-MOCK: selectable concurrency policy shape. `LockBased` is the
/// only implemented policy today and remains store-local.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConcurrencyControl {
    LockBased,
}

/// SHISOFT-TWASM-MOCK: durability is still volatile rollback-only; research
/// `NVMemory` can model PMEM-style flush/fence behavior, but true durability
/// and crash recovery are future work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DurabilityPolicy {
    VolatileRollbackOnly,
}

/// SHISOFT-TWASM-MOCK: conflict behavior is the current abort/default scaffold;
/// richer policy selection is deferred to the real concurrency-control layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConflictPolicy {
    AbortOrWizardDefault,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectIndexPersistencePolicy {
    RebuildOnRecovery,
    PersistentIndex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TransactionConfig {
    tmemory_backend: TMemoryBackend,
    tmemory_persistence_mode: TMemoryPersistenceMode,
    tmemory_file_backing: Option<TMemoryFileBacking>,
    concurrency_control: ConcurrencyControl,
    durability_policy: DurabilityPolicy,
    conflict_policy: ConflictPolicy,
    object_index_persistence_policy: ObjectIndexPersistencePolicy,
}

impl Default for TransactionConfig {
    fn default() -> Self {
        Self {
            tmemory_backend: TMemoryBackend::VMemory,
            tmemory_persistence_mode: TMemoryPersistenceMode::ResearchPretendPmem,
            tmemory_file_backing: None,
            concurrency_control: ConcurrencyControl::LockBased,
            durability_policy: DurabilityPolicy::VolatileRollbackOnly,
            conflict_policy: ConflictPolicy::AbortOrWizardDefault,
            object_index_persistence_policy: ObjectIndexPersistencePolicy::RebuildOnRecovery,
        }
    }
}

impl TransactionConfig {
    pub(crate) fn with_tmemory_backend(tmemory_backend: TMemoryBackend) -> Result<Self> {
        let mut config = Self::default();
        config.set_tmemory_backend(tmemory_backend)?;
        Ok(config)
    }

    pub(crate) fn with_nvmemory_persistence_mode(
        tmemory_persistence_mode: TMemoryPersistenceMode,
    ) -> Result<Self> {
        let mut config = Self::default();
        config.set_nvmemory_persistence_mode(tmemory_persistence_mode)?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_temp() -> Result<Self> {
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Temp)?;
        Ok(config)
    }

    pub(crate) fn with_file_backed_tmemory_path(path: PathBuf) -> Result<Self> {
        ensure!(
            !path.as_os_str().is_empty(),
            "file-backed tmemory path cannot be empty"
        );
        let mut config = Self::default();
        config.set_file_backed_tmemory(TMemoryFileBacking::Path(path))?;
        Ok(config)
    }

    pub(crate) fn with_object_index_persistence_policy(
        object_index_persistence_policy: ObjectIndexPersistencePolicy,
    ) -> Result<Self> {
        let mut config = Self::default();
        config.set_object_index_persistence_policy(object_index_persistence_policy)?;
        Ok(config)
    }

    pub(crate) fn tmemory_backend(&self) -> TMemoryBackend {
        self.tmemory_backend
    }

    pub(crate) fn tmemory_persistence_mode(&self) -> TMemoryPersistenceMode {
        self.tmemory_persistence_mode
    }

    pub(crate) fn tmemory_file_backing(&self) -> Option<TMemoryFileBacking> {
        self.tmemory_file_backing.clone()
    }

    pub(crate) fn concurrency_control(&self) -> ConcurrencyControl {
        self.concurrency_control
    }

    pub(crate) fn durability_policy(&self) -> DurabilityPolicy {
        self.durability_policy
    }

    pub(crate) fn conflict_policy(&self) -> ConflictPolicy {
        self.conflict_policy
    }

    pub(crate) fn object_index_persistence_policy(&self) -> ObjectIndexPersistencePolicy {
        self.object_index_persistence_policy
    }

    pub(crate) fn is_vmemory_only(&self) -> bool {
        self.tmemory_backend == TMemoryBackend::VMemory
    }

    fn set_tmemory_backend(&mut self, tmemory_backend: TMemoryBackend) -> Result<()> {
        match tmemory_backend {
            TMemoryBackend::VMemory | TMemoryBackend::NVMemory => {
                self.tmemory_backend = tmemory_backend;
                self.tmemory_file_backing = None;
                Ok(())
            }
            TMemoryBackend::FileBackedMemory => {
                bail!("FileBackedMemory requires explicit file backing configuration")
            }
        }
    }

    fn set_nvmemory_persistence_mode(
        &mut self,
        tmemory_persistence_mode: TMemoryPersistenceMode,
    ) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::NVMemory;
        self.tmemory_persistence_mode = tmemory_persistence_mode;
        self.tmemory_file_backing = None;
        Ok(())
    }

    fn set_file_backed_tmemory(&mut self, file_backing: TMemoryFileBacking) -> Result<()> {
        self.tmemory_backend = TMemoryBackend::FileBackedMemory;
        self.tmemory_file_backing = Some(file_backing);
        Ok(())
    }

    fn set_object_index_persistence_policy(
        &mut self,
        object_index_persistence_policy: ObjectIndexPersistencePolicy,
    ) -> Result<()> {
        match object_index_persistence_policy {
            ObjectIndexPersistencePolicy::RebuildOnRecovery => {
                self.object_index_persistence_policy = object_index_persistence_policy;
                Ok(())
            }
            ObjectIndexPersistencePolicy::PersistentIndex => {
                bail!("PersistentIndex object-table policy is not implemented yet")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct TransactionId(u64);

std::thread_local! {
    static CURRENT_TRANSACTION: Cell<Option<TransactionId>> = const { Cell::new(None) };
}

fn current_thread_transaction() -> Option<TransactionId> {
    CURRENT_TRANSACTION.with(Cell::get)
}

fn replace_current_thread_transaction(transaction: Option<TransactionId>) -> Option<TransactionId> {
    CURRENT_TRANSACTION.with(|current| {
        let previous = current.get();
        current.set(transaction);
        previous
    })
}

#[cfg(test)]
fn current_thread_transaction_for_test() -> Option<TransactionId> {
    current_thread_transaction()
}

#[cfg(test)]
fn clear_current_thread_transaction_for_test() {
    replace_current_thread_transaction(None);
}

impl TransactionId {
    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct TransactionState {
    active: Option<TransactionId>,
    failed: bool,
    failure_code: u32,
    next_id: u64,
    locks: LockBased,
    suspended: BTreeMap<TransactionId, TransactionWorkspace>,
    staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    staged_memory_sizes: BTreeMap<GranuleId, u64>,
    staged_table_sizes: BTreeMap<GranuleId, u64>,
    staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    original_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    staged_objects: BTreeMap<ObjectId, ObjectPayload>,
    allocated_objects: Vec<ObjectId>,
    granule_versions: BTreeMap<GranuleId, u64>,
    read_granules: BTreeSet<GranuleId>,
    write_granules: BTreeSet<GranuleId>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
    durable_log: TxDurableLog,
}

#[derive(Debug, Default)]
struct TransactionWorkspace {
    staged_globals: BTreeMap<GranuleId, GlobalSnapshot>,
    staged_granules: BTreeMap<GranuleId, Vec<u8>>,
    staged_memory_sizes: BTreeMap<GranuleId, u64>,
    staged_table_sizes: BTreeMap<GranuleId, u64>,
    staged_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    original_table_elements: BTreeMap<TableElementKey, TableElementSnapshot>,
    staged_objects: BTreeMap<ObjectId, ObjectPayload>,
    allocated_objects: Vec<ObjectId>,
    read_granules: BTreeSet<GranuleId>,
    write_granules: BTreeSet<GranuleId>,
    scratch: Vec<u8>,
    pending_memory_store: Option<PendingMemoryStore>,
}

impl Default for TransactionState {
    fn default() -> Self {
        Self {
            active: None,
            failed: false,
            failure_code: 0,
            next_id: 10_001,
            locks: LockBased::default(),
            suspended: BTreeMap::new(),
            staged_globals: BTreeMap::new(),
            staged_granules: BTreeMap::new(),
            staged_memory_sizes: BTreeMap::new(),
            staged_table_sizes: BTreeMap::new(),
            staged_table_elements: BTreeMap::new(),
            original_table_elements: BTreeMap::new(),
            staged_objects: BTreeMap::new(),
            allocated_objects: Vec::new(),
            granule_versions: BTreeMap::new(),
            read_granules: BTreeSet::new(),
            write_granules: BTreeSet::new(),
            scratch: Vec::new(),
            pending_memory_store: None,
            durable_log: TxDurableLog::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StagedRecord {
    Global {
        owner_instance: Option<InstanceId>,
        global_index: u32,
        value: GlobalSnapshot,
    },
    MemoryGranule {
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    },
    MemorySize {
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        new_pages: u64,
    },
    TableSize {
        owner_instance: Option<InstanceId>,
        table_index: u32,
        new_elements: u64,
    },
    TableElement {
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        value: TableElementSnapshot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GlobalSnapshot {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
    /// SHISOFT-TWASM-MOCK: reference snapshots are raw Wasmtime reference
    /// words for `tglobal.get` execution. Non-null staged reference globals
    /// still need a rooted/object-table snapshot before `tglobal.set` is
    /// enabled for references.
    GcRef(u32),
    FuncRef(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TableElementSnapshot {
    /// Raw `VMFuncRef` pointer address, with `0` representing null.
    FuncRef(usize),
    /// Raw nullable `VMGcRef` word. This is a volatile scaffold until
    /// persistent references use `ObjectId`.
    GcRef(u32),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ObjectId {
    pub(crate) object_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct I31Value(u32);

impl I31Value {
    const MASK: u32 = 0x7fff_ffff;

    fn new(value: i32) -> Self {
        Self((value as u32) & Self::MASK)
    }

    pub(crate) fn get_s(self) -> i32 {
        ((self.0 << 1) as i32) >> 1
    }

    pub(crate) fn get_u(self) -> i32 {
        i32::try_from(self.0).unwrap()
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectKind {
    Struct,
    Array,
    I31,
    Extern,
    Func,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObjectValue {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
    Ref(Option<ObjectId>),
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectRefValue(u64);

impl ObjectRefValue {
    pub(crate) fn from_optional_object_id(object_id: Option<ObjectId>) -> Result<Self> {
        Ok(match object_id {
            Some(object_id) => Self(
                object_id
                    .object_index
                    .checked_add(1)
                    .context("object reference encoding overflow")?,
            ),
            None => Self(0),
        })
    }

    pub(crate) fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn as_raw(self) -> u64 {
        self.0
    }

    pub(crate) fn decode(self) -> Option<ObjectId> {
        self.0
            .checked_sub(1)
            .map(|object_index| ObjectId { object_index })
    }
}

pub(crate) const OBJECT_VALUE_ABI_TAG_I32: u32 = 0;
pub(crate) const OBJECT_VALUE_ABI_TAG_I64: u32 = 1;
pub(crate) const OBJECT_VALUE_ABI_TAG_F32: u32 = 2;
pub(crate) const OBJECT_VALUE_ABI_TAG_F64: u32 = 3;
pub(crate) const OBJECT_VALUE_ABI_TAG_V128: u32 = 4;
pub(crate) const OBJECT_VALUE_ABI_TAG_REF: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectValueAbi {
    tag: u32,
    low: u64,
    high: u64,
}

impl ObjectValueAbi {
    pub(crate) fn from_parts(tag: u32, low: u64, high: u64) -> Result<Self> {
        match tag {
            OBJECT_VALUE_ABI_TAG_I32 => ensure!(
                high == 0 && low <= u64::from(u32::MAX),
                "non-canonical i32 object value ABI payload"
            ),
            OBJECT_VALUE_ABI_TAG_I64 => {
                ensure!(high == 0, "non-canonical i64 object value ABI payload")
            }
            OBJECT_VALUE_ABI_TAG_F32 => ensure!(
                high == 0 && low <= u64::from(u32::MAX),
                "non-canonical f32 object value ABI payload"
            ),
            OBJECT_VALUE_ABI_TAG_F64 => {
                ensure!(high == 0, "non-canonical f64 object value ABI payload")
            }
            OBJECT_VALUE_ABI_TAG_V128 => {}
            OBJECT_VALUE_ABI_TAG_REF => {
                ensure!(high == 0, "non-canonical ref object value ABI payload")
            }
            _ => bail!("unknown object value ABI tag: {tag}"),
        }
        Ok(Self { tag, low, high })
    }

    pub(crate) fn as_parts(self) -> (u32, u64, u64) {
        (self.tag, self.low, self.high)
    }

    pub(crate) fn from_object_value(value: &ObjectValue) -> Result<Self> {
        match value {
            ObjectValue::I32(value) => {
                Self::from_parts(OBJECT_VALUE_ABI_TAG_I32, u64::from(*value as u32), 0)
            }
            ObjectValue::I64(value) => Self::from_parts(OBJECT_VALUE_ABI_TAG_I64, *value as u64, 0),
            ObjectValue::F32(value) => {
                Self::from_parts(OBJECT_VALUE_ABI_TAG_F32, u64::from(*value), 0)
            }
            ObjectValue::F64(value) => Self::from_parts(OBJECT_VALUE_ABI_TAG_F64, *value, 0),
            ObjectValue::V128(value) => {
                let low = u64::from_le_bytes(value[0..8].try_into().unwrap());
                let high = u64::from_le_bytes(value[8..16].try_into().unwrap());
                Self::from_parts(OBJECT_VALUE_ABI_TAG_V128, low, high)
            }
            ObjectValue::Ref(object_id) => Self::from_parts(
                OBJECT_VALUE_ABI_TAG_REF,
                ObjectRefValue::from_optional_object_id(*object_id)?.as_raw(),
                0,
            ),
        }
    }

    pub(crate) fn to_object_value(self) -> Result<ObjectValue> {
        let Self { tag, low, high } = Self::from_parts(self.tag, self.low, self.high)?;
        Ok(match tag {
            OBJECT_VALUE_ABI_TAG_I32 => ObjectValue::I32(low as u32 as i32),
            OBJECT_VALUE_ABI_TAG_I64 => ObjectValue::I64(low as i64),
            OBJECT_VALUE_ABI_TAG_F32 => ObjectValue::F32(low as u32),
            OBJECT_VALUE_ABI_TAG_F64 => ObjectValue::F64(low),
            OBJECT_VALUE_ABI_TAG_V128 => {
                let mut bytes = [0; 16];
                bytes[0..8].copy_from_slice(&low.to_le_bytes());
                bytes[8..16].copy_from_slice(&high.to_le_bytes());
                ObjectValue::V128(bytes)
            }
            OBJECT_VALUE_ABI_TAG_REF => ObjectValue::Ref(ObjectRefValue::from_raw(low).decode()),
            _ => unreachable!(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ObjectPayload {
    Struct(Vec<ObjectValue>),
    Array(Vec<ObjectValue>),
    I31(i32),
    Extern(u64),
    // The payload stores Wasmtime/module-local function metadata. The function
    // object's identity is the `ObjectId` table slot that owns this payload.
    Func(u64),
}

impl ObjectPayload {
    fn kind(&self) -> ObjectKind {
        match self {
            ObjectPayload::Struct(_) => ObjectKind::Struct,
            ObjectPayload::Array(_) => ObjectKind::Array,
            ObjectPayload::I31(_) => ObjectKind::I31,
            ObjectPayload::Extern(_) => ObjectKind::Extern,
            ObjectPayload::Func(_) => ObjectKind::Func,
        }
    }

    fn default_for_kind(kind: ObjectKind) -> Self {
        match kind {
            ObjectKind::Struct => ObjectPayload::Struct(Vec::new()),
            ObjectKind::Array => ObjectPayload::Array(Vec::new()),
            ObjectKind::I31 => ObjectPayload::I31(0),
            ObjectKind::Extern => ObjectPayload::Extern(0),
            ObjectKind::Func => ObjectPayload::Func(0),
        }
    }
}

fn object_kind_from_u16(raw: u16) -> Result<ObjectKind> {
    match raw {
        x if x == ObjectKind::Struct as u16 => Ok(ObjectKind::Struct),
        x if x == ObjectKind::Array as u16 => Ok(ObjectKind::Array),
        x if x == ObjectKind::I31 as u16 => Ok(ObjectKind::I31),
        x if x == ObjectKind::Extern as u16 => Ok(ObjectKind::Extern),
        x if x == ObjectKind::Func as u16 => Ok(ObjectKind::Func),
        _ => bail!("unknown object kind tag in persistent record header: {raw}"),
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ObjectTableSlot {
    kind: ObjectKind,
    version: u64,
    type_index: u32,
    persistent: bool,
    current_record: object_heap::TxRecordHandle,
}

#[derive(Debug, Default)]
pub(crate) struct ObjectTable {
    slots: Vec<Option<ObjectTableSlot>>,
    free_list: Vec<ObjectId>,
    // SHISOFT-TWASM-MOCK: these volatile side maps let the first transactional
    // object libcalls use Wasmtime GC refs and VMFuncRef pointers while the
    // persistent-object runtime is still coming online. The final form uses
    // `ObjectId` as the canonical identifier for persistent objects and
    // transactional function objects; raw Wasmtime refs are only live wrappers
    // around that identity.
    gc_ref_to_object: BTreeMap<u32, ObjectId>,
    object_to_gc_ref: BTreeMap<ObjectId, u32>,
    func_ref_to_object: BTreeMap<u64, ObjectId>,
    object_to_func_ref: BTreeMap<ObjectId, u64>,
    next_version: u64,
    next_record_version: u32,
    live_count: usize,
    heap: object_heap::ObjectHeap,
}

impl ObjectTable {
    pub(crate) fn allocate(&mut self, kind: ObjectKind) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::default_for_kind(kind))
    }

    pub(crate) fn allocate_struct(&mut self, fields: Vec<ObjectValue>) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Struct(fields))
    }

    pub(crate) fn allocate_persistent_struct_for_gc_ref(
        &mut self,
        gc_ref: u32,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            gc_ref != 0,
            "transactional struct object cannot use null GC ref"
        );
        ensure!(
            !self.gc_ref_to_object.contains_key(&gc_ref),
            "transactional object GC ref is already associated"
        );
        let object_id =
            self.allocate_payload_with_persistence(ObjectPayload::Struct(fields), true)?;
        self.associate_gc_ref(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_struct_for_gc_ref(
        &mut self,
        gc_ref: u32,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            gc_ref != 0,
            "transactional struct object cannot use null GC ref"
        );
        ensure!(
            !self.gc_ref_to_object.contains_key(&gc_ref),
            "transactional object GC ref is already associated"
        );
        let object_id = self.allocate_struct(fields)?;
        self.associate_gc_ref(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_array(&mut self, elements: Vec<ObjectValue>) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Array(elements))
    }

    pub(crate) fn allocate_persistent_array_for_gc_ref(
        &mut self,
        gc_ref: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            gc_ref != 0,
            "transactional array object cannot use null GC ref"
        );
        ensure!(
            !self.gc_ref_to_object.contains_key(&gc_ref),
            "transactional object GC ref is already associated"
        );
        let object_id =
            self.allocate_payload_with_persistence(ObjectPayload::Array(elements), true)?;
        self.associate_gc_ref(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_array_for_gc_ref(
        &mut self,
        gc_ref: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            gc_ref != 0,
            "transactional array object cannot use null GC ref"
        );
        ensure!(
            !self.gc_ref_to_object.contains_key(&gc_ref),
            "transactional object GC ref is already associated"
        );
        let object_id = self.allocate_array(elements)?;
        self.associate_gc_ref(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn object_id_for_raw_ref_or_func(&mut self, raw_ref: u64) -> Result<ObjectId> {
        ensure!(raw_ref != 0, "transactional object cannot use null ref");
        if let Ok(gc_ref) = u32::try_from(raw_ref)
            && let Some(object_id) = self.gc_ref_to_object.get(&gc_ref).copied()
        {
            return Ok(object_id);
        }
        if let Some(object_id) = self.func_ref_to_object.get(&raw_ref).copied() {
            return Ok(object_id);
        }

        let object_id = self.allocate_payload(ObjectPayload::Func(raw_ref))?;
        self.associate_func_ref(raw_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_payload(&mut self, payload: ObjectPayload) -> Result<ObjectId> {
        self.allocate_payload_with_persistence(payload, false)
    }

    fn allocate_payload_with_persistence(
        &mut self,
        payload: ObjectPayload,
        persistent: bool,
    ) -> Result<ObjectId> {
        let reused_slot = self.free_list.last().copied();
        let object_id = match reused_slot {
            Some(object_id) => object_id,
            None => ObjectId {
                object_index: u64::try_from(self.slots.len())
                    .context("object table slot count does not fit u64")?,
            },
        };
        let index = object_slot_index(object_id)?;
        if index == self.slots.len() {
            self.slots.push(None);
        }
        ensure!(
            index < self.slots.len(),
            "object table freelist entry is outside slot range"
        );
        ensure!(
            self.slots[index].is_none(),
            "object table freelist entry points at a live slot"
        );
        let kind = payload.kind();
        let record_version = self.bump_record_version()?;
        let record = self
            .heap
            .allocate_record(object_id, record_version, kind, 0, 0, &payload)?;
        let version = self.bump_object_version()?;
        if reused_slot.is_some() {
            let _ = self.free_list.pop();
        }
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_index: 0,
            persistent,
            current_record: record,
        });
        self.live_count = self
            .live_count
            .checked_add(1)
            .context("object table live count overflow")?;
        Ok(object_id)
    }

    pub(crate) fn live_count(&self) -> usize {
        self.live_count
    }

    pub(crate) fn associate_gc_ref(&mut self, gc_ref: u32, object_id: ObjectId) -> Result<()> {
        ensure!(gc_ref != 0, "transactional object cannot use null GC ref");
        self.live_slot(object_id)?;
        if let Some(existing) = self.gc_ref_to_object.get(&gc_ref).copied() {
            ensure!(
                existing == object_id,
                "transactional object GC ref is already associated"
            );
        }
        if let Some(existing) = self.object_to_gc_ref.get(&object_id).copied() {
            ensure!(
                existing == gc_ref,
                "transactional object id is already associated with another GC ref"
            );
        }
        self.gc_ref_to_object.insert(gc_ref, object_id);
        self.object_to_gc_ref.insert(object_id, gc_ref);
        Ok(())
    }

    pub(crate) fn associate_func_ref(&mut self, func_ref: u64, object_id: ObjectId) -> Result<()> {
        ensure!(
            func_ref != 0,
            "transactional function object cannot use null VMFuncRef"
        );
        ensure!(
            self.kind(object_id)? == ObjectKind::Func,
            "transactional function ref can only be associated with function objects"
        );
        if let Some(existing) = self.func_ref_to_object.get(&func_ref).copied() {
            ensure!(
                existing == object_id,
                "transactional function ref is already associated"
            );
        }
        if let Some(existing) = self.object_to_func_ref.get(&object_id).copied() {
            ensure!(
                existing == func_ref,
                "transactional function object is already associated with another VMFuncRef"
            );
        }
        self.func_ref_to_object.insert(func_ref, object_id);
        self.object_to_func_ref.insert(object_id, func_ref);
        Ok(())
    }

    pub(crate) fn object_id_for_gc_ref(&self, gc_ref: u32) -> Result<ObjectId> {
        ensure!(gc_ref != 0, "transactional object cannot use null GC ref");
        self.gc_ref_to_object
            .get(&gc_ref)
            .copied()
            .with_context(|| format!("unknown transactional object GC ref: {gc_ref:#x}"))
    }

    pub(crate) fn known_object_id_for_gc_ref(&self, gc_ref: u32) -> Option<ObjectId> {
        if gc_ref == 0 {
            return None;
        }
        self.gc_ref_to_object.get(&gc_ref).copied()
    }

    pub(crate) fn known_persistent_object_id_for_gc_ref(
        &self,
        gc_ref: u32,
    ) -> Result<Option<ObjectId>> {
        let Some(object_id) = self.known_object_id_for_gc_ref(gc_ref) else {
            return Ok(None);
        };
        if self.is_persistent(object_id)? {
            Ok(Some(object_id))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn gc_ref_for_object_id(&self, object_id: ObjectId) -> Result<u32> {
        self.live_slot(object_id)?;
        self.object_to_gc_ref
            .get(&object_id)
            .copied()
            .with_context(|| {
                format!("transactional object has no GC ref association: {object_id:?}")
            })
    }

    pub(crate) fn raw_ref_for_object_id(&self, object_id: ObjectId) -> Result<u64> {
        self.live_slot(object_id)?;
        if let Some(func_ref) = self.object_to_func_ref.get(&object_id).copied() {
            return Ok(func_ref);
        }
        Ok(u64::from(self.gc_ref_for_object_id(object_id)?))
    }

    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn kind(&self, object_id: ObjectId) -> Result<ObjectKind> {
        Ok(self.live_slot(object_id)?.kind)
    }

    pub(crate) fn version(&self, object_id: ObjectId) -> Result<u64> {
        Ok(self.live_slot(object_id)?.version)
    }

    pub(crate) fn is_persistent(&self, object_id: ObjectId) -> Result<bool> {
        Ok(self.live_slot(object_id)?.persistent)
    }

    pub(crate) fn payload(&self, object_id: ObjectId) -> Result<ObjectPayload> {
        let slot = self.live_slot(object_id)?;
        Ok(self.heap.payload(slot.current_record)?.clone())
    }

    pub(crate) fn trace_object_ids(&self, object_id: ObjectId) -> Result<Vec<ObjectId>> {
        let slot = self.live_slot(object_id)?;
        self.heap.trace_object_ids(slot.current_record)
    }

    pub(crate) fn update_payload(
        &mut self,
        object_id: ObjectId,
        payload: ObjectPayload,
    ) -> Result<()> {
        let index = object_slot_index(object_id)?;
        let kind = self.live_slot(object_id)?.kind;
        ensure!(
            kind == payload.kind(),
            "object payload kind does not match object table slot kind"
        );
        let type_index = self.live_slot(object_id)?.type_index;
        let persistent = self.live_slot(object_id)?.persistent;
        let record_version = self.bump_record_version()?;
        let record =
            self.heap
                .allocate_record(object_id, record_version, kind, 0, type_index, &payload)?;
        let version = self.bump_object_version()?;
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_index,
            persistent,
            current_record: record,
        });
        Ok(())
    }

    pub(crate) fn granule_id(&self, object_id: ObjectId) -> Result<GranuleId> {
        match self.kind(object_id)? {
            ObjectKind::Struct => Ok(GranuleId::TStruct { object_id }),
            ObjectKind::Array => Ok(GranuleId::TArray { object_id }),
            ObjectKind::I31 | ObjectKind::Extern | ObjectKind::Func => {
                bail!("object kind does not have a transactional granule yet")
            }
        }
    }

    fn object_pending_publication(
        &self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        let handle = self.live_slot(object_id)?.current_record;
        let (header, payload) = self.heap.publication_record(handle)?;
        let domain = match object_kind_from_u16(header.kind)? {
            ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
            ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
            other => bail!("object kind {other:?} is not a persistent object granule"),
        };
        persist::PendingPublication::persistent_object(
            domain,
            header.object_id,
            header.version,
            header.type_index,
            payload,
        )
    }

    pub(crate) fn free(&mut self, object_id: ObjectId) -> Result<bool> {
        let index = object_slot_index(object_id)?;
        if index >= self.slots.len() {
            return Ok(false);
        }
        if self.slots[index].is_none() {
            return Ok(false);
        }
        self.slots[index] = None;
        if let Some(gc_ref) = self.object_to_gc_ref.remove(&object_id) {
            self.gc_ref_to_object.remove(&gc_ref);
        }
        if let Some(func_ref) = self.object_to_func_ref.remove(&object_id) {
            self.func_ref_to_object.remove(&func_ref);
        }
        self.bump_object_version()?;
        self.free_list.push(object_id);
        self.live_count = self
            .live_count
            .checked_sub(1)
            .context("object table live count underflow")?;
        Ok(true)
    }

    fn live_slot(&self, object_id: ObjectId) -> Result<&ObjectTableSlot> {
        let index = object_slot_index(object_id)?;
        self.slots
            .get(index)
            .and_then(Option::as_ref)
            .with_context(|| format!("object table slot is not live: {object_id:?}"))
    }

    fn bump_object_version(&mut self) -> Result<u64> {
        self.next_version = self
            .next_version
            .checked_add(1)
            .context("object table version overflow")?;
        Ok(self.next_version)
    }

    fn bump_record_version(&mut self) -> Result<u32> {
        self.next_record_version = self
            .next_record_version
            .checked_add(1)
            .context("object record version overflow")?;
        Ok(self.next_record_version)
    }

    fn clear_volatile_index(&mut self) {
        self.slots.clear();
        self.free_list.clear();
        self.gc_ref_to_object.clear();
        self.object_to_gc_ref.clear();
        self.func_ref_to_object.clear();
        self.object_to_func_ref.clear();
        self.next_version = 0;
        self.next_record_version = 0;
        self.live_count = 0;
    }

    fn rebuild_free_list_holes(&mut self) -> Result<()> {
        self.free_list.clear();
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.is_none() {
                self.free_list.push(ObjectId {
                    object_index: u64::try_from(index).context("slot index does not fit u64")?,
                });
            }
        }
        Ok(())
    }

    fn rebuild_volatile_index_from_heap(&mut self) -> Result<()> {
        self.clear_volatile_index();

        let mut latest_by_object =
            BTreeMap::<ObjectId, (object_heap::TxRecordHandle, object_heap::TxObjectHeader)>::new();
        for (handle, header) in self.heap.published_records() {
            let object_id = ObjectId {
                object_index: header.object_id,
            };
            self.next_record_version = self.next_record_version.max(header.version);
            match latest_by_object.get(&object_id) {
                Some((_, current)) if current.version >= header.version => {}
                _ => {
                    latest_by_object.insert(object_id, (handle, header));
                }
            }
        }

        if let Some(max_object_id) = latest_by_object.keys().map(|id| id.object_index).max() {
            let slot_len = usize::try_from(
                max_object_id
                    .checked_add(1)
                    .context("object slot range overflow")?,
            )
            .context("object slot range does not fit usize")?;
            self.slots.resize(slot_len, None);
        }

        for (object_id, (handle, header)) in latest_by_object {
            let index = object_slot_index(object_id)?;
            let kind = object_kind_from_u16(header.kind)?;
            let version = self.bump_object_version()?;
            self.slots[index] = Some(ObjectTableSlot {
                kind,
                version,
                type_index: header.type_index,
                persistent: true,
                current_record: handle,
            });
            self.live_count = self
                .live_count
                .checked_add(1)
                .context("object table live count overflow during rebuild")?;
        }

        self.rebuild_free_list_holes()?;
        Ok(())
    }

    fn rebuild_from_recovered_object_winners(
        &mut self,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
    ) -> Result<()> {
        self.clear_volatile_index();
        self.heap = object_heap::ObjectHeap::default();

        if let Some(max_object_id) = winners.iter().map(|winner| winner.object_id).max() {
            let slot_len = usize::try_from(
                max_object_id
                    .checked_add(1)
                    .context("object slot range overflow")?,
            )
            .context("object slot range does not fit usize")?;
            self.slots.resize(slot_len, None);
        }

        for winner in winners {
            let object_id = ObjectId {
                object_index: winner.object_id,
            };
            let index = object_slot_index(object_id)?;
            ensure!(
                self.slots.get(index).is_some(),
                "recovered object slot is outside slot range"
            );
            ensure!(
                self.slots[index].is_none(),
                "recovered object slot is already occupied"
            );

            let handle = self.heap.install_record_bytes(&winner.record_bytes)?;
            let header = self.heap.header(handle)?;
            let kind = object_kind_from_u16(header.kind)?;

            ensure!(
                header.object_id == winner.object_id,
                "recovered object record id does not match winner"
            );
            ensure!(
                header.version == winner.version,
                "recovered object record version does not match winner"
            );
            ensure!(
                header.kind == winner.kind,
                "recovered object record kind does not match winner"
            );
            ensure!(
                header.type_index == winner.type_index,
                "recovered object record type index does not match winner"
            );

            self.next_record_version = self.next_record_version.max(header.version);
            let version = self.bump_object_version()?;
            self.slots[index] = Some(ObjectTableSlot {
                kind,
                version,
                type_index: header.type_index,
                persistent: true,
                current_record: handle,
            });
            self.live_count = self
                .live_count
                .checked_add(1)
                .context("object table live count overflow during recovery rebuild")?;
        }

        self.rebuild_free_list_holes()?;
        Ok(())
    }

    #[cfg(test)]
    fn current_record_handle_for_test(
        &self,
        object_id: ObjectId,
    ) -> Result<object_heap::TxRecordHandle> {
        Ok(self.live_slot(object_id)?.current_record)
    }

    #[cfg(test)]
    fn pending_publication_for_test(
        &self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        self.object_pending_publication(object_id)
    }

    #[cfg(test)]
    fn rebuild_from_recovery_for_test(
        &mut self,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
    ) -> Result<()> {
        self.rebuild_from_recovered_object_winners(winners)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum GranuleId {
    TMemory {
        instance: Option<u32>,
        memory_index: u32,
        granule_index: u64,
    },
    TMemorySize {
        instance: Option<u32>,
        memory_index: u32,
    },
    TGlobal {
        instance: Option<u32>,
        global_index: u32,
    },
    TTable {
        instance: Option<u32>,
        table_index: u32,
        granule_index: u64,
    },
    TTableSize {
        instance: Option<u32>,
        table_index: u32,
    },
    // Future object-table identities; not wired into runtime paths yet.
    #[allow(dead_code)]
    TStruct { object_id: ObjectId },
    #[allow(dead_code)]
    TArray { object_id: ObjectId },
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TableElementKey {
    instance: Option<u32>,
    table_index: u32,
    element_index: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingMemoryStore {
    instance: InstanceId,
    memory_index: u32,
    addr: u64,
    len: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryGranuleSnapshot {
    granule_index: usize,
    range: Range<usize>,
    version: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct TMemoryAccessSnapshot {
    base: u64,
    bytes: Vec<u8>,
    byte_len: usize,
    granules: Vec<TMemoryGranuleSnapshot>,
}

pub(crate) const TMEMORY_GRANULE_SHIFT: usize = 6;
pub(crate) const TMEMORY_GRANULE_SIZE: usize = 1 << TMEMORY_GRANULE_SHIFT;
const TTABLE_GRANULE_SHIFT: u32 = 4;
pub(crate) const TTABLE_GRANULE_SIZE: u64 = 1 << TTABLE_GRANULE_SHIFT;

pub(crate) trait TransactionConcurrencyControl {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>>;

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>>;

    fn release_transaction(&mut self, transaction: TransactionId);
}

#[derive(Debug, Default)]
pub(crate) struct LockBased {
    // SHISOFT-TWASM-MOCK: versioned persistent backends are still incomplete,
    // so some non-object granules feed version `0` through the lock manager.
    owners: BTreeMap<GranuleId, TransactionId>,
    read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LockBasedConflictKind {
    ReadOwnedByOther,
    ReadVersionMismatch,
    WriteOwnedByOther,
    WriteVersionMismatch,
}

impl LockBasedConflictKind {
    fn message(self) -> &'static str {
        match self {
            Self::ReadOwnedByOther => {
                "transaction read conflict: granule is owned by another transaction"
            }
            Self::ReadVersionMismatch => {
                "transaction read conflict: optimistic read version changed"
            }
            Self::WriteOwnedByOther => {
                "transaction write conflict: granule is owned by another transaction"
            }
            Self::WriteVersionMismatch => {
                "transaction write conflict: optimistic read version changed"
            }
        }
    }
}

#[cfg(test)]
use self::LockBasedConflictKind as LockBasedConflictKindForTest;

impl LockBased {
    pub(crate) fn record_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<Option<TransactionId>> {
        Self::map_conflict_result(self.record_read_typed(transaction, granule, version))
    }

    pub(crate) fn acquire_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        Self::map_conflict_result(self.acquire_write_typed(transaction, granule, current_version))
    }

    pub(crate) fn validate_read(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        Self::map_conflict_result(self.validate_read_typed(transaction, granule, current_version))
    }

    pub(crate) fn validate_transaction_reads<F>(
        &self,
        transaction: TransactionId,
        mut current_version_fn: F,
    ) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        for ((reader, granule), _) in self.read_versions.iter() {
            if *reader != transaction {
                continue;
            }
            let current_version = current_version_fn(*granule)?;
            self.validate_read(transaction, *granule, current_version)?;
        }

        Ok(())
    }

    fn refresh_read_version(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) {
        if let Some(version) = self.read_versions.get_mut(&(transaction, granule)) {
            *version = current_version;
        }
    }

    pub(crate) fn release_transaction(&mut self, transaction: TransactionId) {
        self.owners.retain(|_, owner| *owner != transaction);
        self.read_versions
            .retain(|(reader, _), _| *reader != transaction);
    }

    fn map_conflict_result<T>(result: core::result::Result<T, LockBasedConflictKind>) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(kind) => bail!(kind.message()),
        }
    }

    fn record_read_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, false)?;

        match self.read_versions.entry((transaction, granule)) {
            alloc::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(version);
            }
            alloc::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() != version {
                    return Err(LockBasedConflictKind::ReadVersionMismatch);
                }
            }
        }

        Ok(aborted)
    }

    fn acquire_write_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        let aborted = self.resolve_writer_conflict_typed(transaction, granule, true)?;
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(LockBasedConflictKind::WriteVersionMismatch);
        }

        self.owners.insert(granule, transaction);
        Ok(aborted)
    }

    fn validate_read_typed(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKind> {
        if self
            .owners
            .get(&granule)
            .is_some_and(|owner| *owner != transaction)
        {
            return Err(LockBasedConflictKind::ReadOwnedByOther);
        }
        if self
            .read_versions
            .get(&(transaction, granule))
            .is_some_and(|version| *version != current_version)
        {
            return Err(LockBasedConflictKind::ReadVersionMismatch);
        }

        Ok(())
    }

    fn resolve_writer_conflict_typed(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        is_write: bool,
    ) -> core::result::Result<Option<TransactionId>, LockBasedConflictKind> {
        let Some(owner) = self.owners.get(&granule).copied() else {
            return Ok(None);
        };
        if owner == transaction {
            return Ok(None);
        }
        if transaction < owner {
            self.release_transaction(owner);
            return Ok(Some(owner));
        }
        Err(if is_write {
            LockBasedConflictKind::WriteOwnedByOther
        } else {
            LockBasedConflictKind::ReadOwnedByOther
        })
    }
}

#[cfg(test)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct LockBasedSnapshotForTest {
    owners: BTreeMap<GranuleId, TransactionId>,
    read_versions: BTreeMap<(TransactionId, GranuleId), u64>,
}

#[cfg(test)]
impl LockBased {
    fn clone_for_test(&self) -> Self {
        Self {
            owners: self.owners.clone(),
            read_versions: self.read_versions.clone(),
        }
    }

    fn snapshot_for_test(&self) -> LockBasedSnapshotForTest {
        LockBasedSnapshotForTest {
            owners: self.owners.clone(),
            read_versions: self.read_versions.clone(),
        }
    }

    fn record_read_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> Result<()> {
        self.record_read(transaction, granule, version)?;
        Ok(())
    }

    fn acquire_write_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.acquire_write(transaction, granule, current_version)?;
        Ok(())
    }

    fn acquire_memory_granule_read(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.acquire_granule_read(
            transaction,
            memory_granule_id_from_u64(None, memory_index, granule_index),
            0,
        )?;
        Ok(())
    }

    fn acquire_memory_granule_write(
        &mut self,
        transaction: TransactionId,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<()> {
        self.acquire_granule_write(
            transaction,
            memory_granule_id_from_u64(None, memory_index, granule_index),
            0,
        )?;
        Ok(())
    }

    fn validate_read_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        self.validate_read(transaction, granule, current_version)
    }

    fn record_read_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.record_read_typed(transaction, granule, version)
            .map(|_| ())
    }

    fn acquire_write_result_for_test(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.acquire_write_typed(transaction, granule, current_version)
            .map(|_| ())
    }

    fn validate_read_result_for_test(
        &self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> core::result::Result<(), LockBasedConflictKindForTest> {
        self.validate_read_typed(transaction, granule, current_version)
    }

    fn abort_for_test(&mut self, transaction: TransactionId) {
        self.release_transaction(transaction);
    }

    fn owner_for_test(&self, granule: GranuleId) -> Option<TransactionId> {
        self.owners.get(&granule).copied()
    }
}

impl TransactionConcurrencyControl for LockBased {
    fn acquire_granule_read(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.record_read(transaction, granule, current_version)
    }

    fn acquire_granule_write(
        &mut self,
        transaction: TransactionId,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<Option<TransactionId>> {
        self.acquire_write(transaction, granule, current_version)
    }

    fn release_transaction(&mut self, transaction: TransactionId) {
        LockBased::release_transaction(self, transaction);
    }
}

impl TransactionState {
    pub(crate) fn begin(&mut self) -> Result<TransactionId> {
        ensure!(
            !self.failed,
            "cannot begin transaction while structured ttry failure is pending"
        );
        ensure!(
            self.active.is_none(),
            "transaction is already active in this store"
        );
        let id = TransactionId::from_raw(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("transaction id overflow")?;
        self.active = Some(id);
        replace_current_thread_transaction(Some(id));
        Ok(id)
    }

    pub(crate) fn enter_transaction(
        &mut self,
        transaction: TransactionId,
    ) -> Result<Option<TransactionId>> {
        ensure!(
            !self.failed,
            "cannot enter transaction while structured ttry failure is pending"
        );
        ensure!(
            self.active != Some(transaction),
            "transaction id is already current"
        );
        let previous = self.suspend_active_workspace()?;
        let workspace = self.suspended.remove(&transaction).unwrap_or_default();
        self.install_workspace(workspace);
        self.active = Some(transaction);
        replace_current_thread_transaction(Some(transaction));
        Ok(previous)
    }

    pub(crate) fn restore_transaction(&mut self, previous: Option<TransactionId>) -> Result<()> {
        self.suspend_active_workspace()?;
        match previous {
            Some(transaction) => {
                let workspace = self
                    .suspended
                    .remove(&transaction)
                    .with_context(|| format!("transaction is not suspended: {transaction:?}"))?;
                self.install_workspace(workspace);
                self.active = Some(transaction);
            }
            None => {
                self.install_workspace(TransactionWorkspace::default());
                self.active = None;
            }
        }
        replace_current_thread_transaction(previous);
        Ok(())
    }

    pub(crate) fn transaction_is_open(&self, transaction: TransactionId) -> bool {
        self.active == Some(transaction) || self.suspended.contains_key(&transaction)
    }

    pub(crate) fn abort_transaction(&mut self, transaction: TransactionId) -> Result<bool> {
        if self.active == Some(transaction) {
            self.abort()?;
            return Ok(true);
        }
        if let Some(workspace) = self.suspended.remove(&transaction) {
            self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
            self.locks.release_transaction(transaction);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn abort_transaction_allocated_objects(
        &mut self,
        object_table: &mut ObjectTable,
        transaction: TransactionId,
    ) -> Result<bool> {
        if self.active == Some(transaction) {
            self.abort_allocated_objects(object_table)?;
            return Ok(true);
        }
        let Some(workspace) = self.suspended.remove(&transaction) else {
            return Ok(false);
        };
        for object_id in workspace.allocated_objects.iter().rev().copied() {
            object_table.free(object_id)?;
        }
        self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
        self.locks.release_transaction(transaction);
        Ok(true)
    }

    pub(crate) fn active_transaction(&self) -> Option<TransactionId> {
        self.active
    }

    pub(crate) fn active_transaction_required_raw(&self) -> Result<u64> {
        Ok(self.active_transaction_required()?.as_raw())
    }

    pub(crate) fn publish_tmemory_undo_before_in_place_write(
        &mut self,
        stream_id: u32,
        txid: u32,
        undo: &PendingGranuleUndo,
    ) -> Result<PendingCommitLogEntry> {
        let mut sink = self.durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        publisher.publish_tmemory_undo_before_in_place_write(undo)
    }

    pub(crate) fn publish_object_publications_before_commit(
        &mut self,
        stream_id: u32,
        txid: u32,
        publications: &[persist::PendingPublication],
    ) -> Result<Option<PendingCommitLogEntry>> {
        let mut sink = self.durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        let mut final_marker = None;
        for publication in publications {
            final_marker = Some(publisher.publish_object_publication_before_commit(publication)?);
        }
        Ok(final_marker)
    }

    pub(crate) fn publish_commit_lp(
        &mut self,
        stream_id: u32,
        txid: u32,
        marker: PendingCommitLogEntry,
    ) -> Result<()> {
        let mut sink = self.durable_log.stream_sink(stream_id);
        let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
        publisher.publish_commit_lp(marker)
    }

    #[cfg(test)]
    pub(crate) fn durable_log_entries_for_test(
        &self,
        stream_id: u32,
    ) -> Vec<crate::vm::TxLogEntry> {
        self.durable_log.log_entries_for_test(stream_id)
    }

    pub(crate) fn structured_failure_pending(&self) -> bool {
        self.failed
    }

    pub(crate) fn structured_failure_code(&self) -> u32 {
        self.failure_code
    }

    pub(crate) fn clear_structured_failure(&mut self) {
        self.failed = false;
        self.failure_code = 0;
    }

    pub(crate) fn acquire_granule_read(
        &mut self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let current_version = self.current_version_for_granule(granule, current_version);
        let aborted = self
            .locks
            .acquire_granule_read(transaction, granule, current_version)?;
        if aborted.is_some() {
            self.discard_conflict_aborted_transaction(aborted)?;
            let current_version = self.current_version_for_granule(granule, current_version);
            self.locks
                .refresh_read_version(transaction, granule, current_version);
        }
        Ok(self.read_granules.insert(granule))
    }

    pub(crate) fn acquire_granule_write(
        &mut self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        let transaction = self.active_transaction_required()?;
        let current_version = self.current_version_for_granule(granule, current_version);
        let aborted = self
            .locks
            .acquire_granule_write(transaction, granule, current_version)?;
        if aborted.is_some() {
            self.discard_conflict_aborted_transaction(aborted)?;
            let current_version = self.current_version_for_granule(granule, current_version);
            self.locks
                .refresh_read_version(transaction, granule, current_version);
        }
        self.read_granules.insert(granule);
        Ok(self.write_granules.insert(granule))
    }

    pub(crate) fn owns_granule_read(&self, granule: GranuleId) -> bool {
        self.read_granules.contains(&granule)
    }

    pub(crate) fn owns_granule_write(&self, granule: GranuleId) -> bool {
        self.write_granules.contains(&granule)
    }

    pub(crate) fn commit(&mut self) -> Result<()> {
        self.commit_with(|_| Ok(()))
    }

    pub(crate) fn commit_with<F>(&mut self, mut apply: F) -> Result<()>
    where
        F: FnMut(&StagedRecord) -> Result<()>,
    {
        self.commit_with_read_validation(|_| Ok(0), |record| apply(record))
    }

    pub(crate) fn commit_with_read_validation<V, F>(
        &mut self,
        current_version_fn: V,
        mut apply: F,
    ) -> Result<()>
    where
        V: FnMut(GranuleId) -> Result<u64>,
        F: FnMut(&StagedRecord) -> Result<()>,
    {
        self.ensure_active()?;
        self.validate_active_reads_with(current_version_fn)?;
        for record in self.staged_records()? {
            apply(&record)?;
        }
        self.complete_commit()
    }

    pub(crate) fn validate_active_reads_with<F>(&self, current_version_fn: F) -> Result<()>
    where
        F: FnMut(GranuleId) -> Result<u64>,
    {
        let transaction = self.active_transaction_required()?;
        self.locks
            .validate_transaction_reads(transaction, current_version_fn)
    }

    pub(crate) fn active_read_granules(&self) -> Result<Vec<GranuleId>> {
        let transaction = self.active_transaction_required()?;
        Ok(self
            .locks
            .read_versions
            .keys()
            .filter_map(|(reader, granule)| (*reader == transaction).then_some(*granule))
            .collect())
    }

    pub(crate) fn validate_active_read(
        &self,
        granule: GranuleId,
        current_version: u64,
    ) -> Result<()> {
        let transaction = self.active_transaction_required()?;
        self.locks
            .validate_read(transaction, granule, current_version)
    }

    pub(crate) fn versioned_granule_version(&self, granule: GranuleId) -> u64 {
        self.granule_versions.get(&granule).copied().unwrap_or(0)
    }

    fn current_version_for_granule(&self, granule: GranuleId, backend_version: u64) -> u64 {
        if granule_uses_transaction_state_version(granule) {
            self.versioned_granule_version(granule)
        } else {
            backend_version
        }
    }

    fn discard_conflict_aborted_transaction(
        &mut self,
        transaction: Option<TransactionId>,
    ) -> Result<()> {
        let Some(transaction) = transaction else {
            return Ok(());
        };
        if let Some(workspace) = self.suspended.remove(&transaction) {
            self.bump_versioned_granules(workspace.write_granules.iter().copied())?;
        }
        Ok(())
    }

    fn bump_active_versioned_write_granules(&mut self) -> Result<()> {
        let granules = self.write_granules.iter().copied().collect::<Vec<_>>();
        self.bump_versioned_granules(granules)
    }

    fn bump_versioned_granules<I>(&mut self, granules: I) -> Result<()>
    where
        I: IntoIterator<Item = GranuleId>,
    {
        for granule in granules {
            if !granule_uses_transaction_state_version(granule) {
                continue;
            }
            let version = self.granule_versions.entry(granule).or_insert(0);
            *version = version
                .checked_add(1)
                .context("transaction granule version overflow")?;
        }
        Ok(())
    }

    pub(crate) fn complete_commit(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.bump_active_versioned_write_granules()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn staged_records(&self) -> Result<Vec<StagedRecord>> {
        self.ensure_active()?;
        let mut records = Vec::new();
        for (key, &value) in &self.staged_globals {
            let GranuleId::TGlobal {
                instance,
                global_index,
            } = *key
            else {
                bail!("staged global map contains non-global key");
            };
            records.push(StagedRecord::Global {
                owner_instance: granule_owner_instance(instance),
                global_index,
                value,
            });
        }
        for (key, bytes) in &self.staged_granules {
            let GranuleId::TMemory {
                instance,
                memory_index,
                granule_index,
            } = *key
            else {
                bail!("staged granule map contains non-memory key");
            };
            records.push(StagedRecord::MemoryGranule {
                owner_instance: granule_owner_instance(instance),
                memory_index,
                granule_index,
                bytes: bytes.clone(),
            });
        }
        for (key, &new_pages) in &self.staged_memory_sizes {
            let GranuleId::TMemorySize {
                instance,
                memory_index,
            } = *key
            else {
                bail!("staged memory size map contains non-size key");
            };
            records.push(StagedRecord::MemorySize {
                owner_instance: granule_owner_instance(instance),
                memory_index,
                new_pages,
            });
        }
        for (key, &new_elements) in &self.staged_table_sizes {
            let GranuleId::TTableSize {
                instance,
                table_index,
            } = *key
            else {
                bail!("staged table size map contains non-table-size key");
            };
            records.push(StagedRecord::TableSize {
                owner_instance: granule_owner_instance(instance),
                table_index,
                new_elements,
            });
        }
        for (key, &value) in &self.staged_table_elements {
            records.push(StagedRecord::TableElement {
                owner_instance: granule_owner_instance(key.instance),
                table_index: key.table_index,
                element_index: key.element_index,
                value,
            });
        }
        Ok(records)
    }

    pub(crate) fn abort(&mut self) -> Result<()> {
        self.ensure_active()?;
        self.bump_active_versioned_write_granules()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn record_allocated_object(&mut self, object_id: ObjectId) -> Result<bool> {
        self.ensure_active()?;
        if self.allocated_objects.contains(&object_id) {
            return Ok(false);
        }
        self.allocated_objects.push(object_id);
        Ok(true)
    }

    pub(crate) fn abort_allocated_objects(&mut self, object_table: &mut ObjectTable) -> Result<()> {
        self.ensure_active()?;
        for object_id in self.allocated_objects.iter().rev().copied() {
            object_table.free(object_id)?;
        }
        self.bump_active_versioned_write_granules()?;
        self.clear_active();
        Ok(())
    }

    pub(crate) fn fail_allocated_objects(&mut self, object_table: &mut ObjectTable) -> Result<()> {
        self.fail_allocated_objects_with_code(object_table, 0)
    }

    pub(crate) fn fail_allocated_objects_with_code(
        &mut self,
        object_table: &mut ObjectTable,
        code: u32,
    ) -> Result<()> {
        self.abort_allocated_objects(object_table)?;
        self.failed = true;
        self.failure_code = code;
        Ok(())
    }

    pub(crate) fn fail(&mut self) -> Result<()> {
        self.abort()
    }

    pub(crate) fn fail_structured(&mut self) -> Result<()> {
        self.fail_structured_with_code(0)
    }

    pub(crate) fn fail_structured_with_code(&mut self, code: u32) -> Result<()> {
        self.abort()?;
        self.failed = true;
        self.failure_code = code;
        Ok(())
    }

    pub(crate) fn stage_memory_size(&mut self, memory_index: u32, new_pages: u64) -> Result<bool> {
        self.stage_memory_size_owned(None, memory_index, new_pages)
    }

    pub(crate) fn stage_memory_size_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        new_pages: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.acquire_granule_write(memory_size_granule_id(owner_instance, memory_index), 0)?;
        Ok(self
            .staged_memory_sizes
            .insert(
                memory_size_granule_id(owner_instance, memory_index),
                new_pages,
            )
            .is_none())
    }

    pub(crate) fn staged_memory_size_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Option<u64> {
        self.staged_memory_sizes
            .get(&memory_size_granule_id(owner_instance, memory_index))
            .copied()
    }

    pub(crate) fn stage_global(
        &mut self,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.stage_global_owned(None, global_index, value)
    }

    pub(crate) fn stage_global_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
        value: GlobalSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = global_granule_id(owner_instance, global_index);
        self.acquire_granule_write(key, 0)?;
        Ok(self.staged_globals.insert(key, value).is_none())
    }

    pub(crate) fn staged_global_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Option<GlobalSnapshot> {
        self.staged_globals
            .get(&global_granule_id(owner_instance, global_index))
            .copied()
    }

    pub(crate) fn stage_table_element_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        value: TableElementSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.acquire_table_granule_write_owned(owner_instance, table_index, element_index, 0)?;
        Ok(self
            .staged_table_elements
            .insert(
                table_element_key(owner_instance, table_index, element_index),
                value,
            )
            .is_none())
    }

    pub(crate) fn stage_table_element_with_original_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        original: TableElementSnapshot,
        value: TableElementSnapshot,
    ) -> Result<bool> {
        let key = table_element_key(owner_instance, table_index, element_index);
        self.original_table_elements.entry(key).or_insert(original);
        self.stage_table_element_owned(owner_instance, table_index, element_index, value)
    }

    pub(crate) fn staged_table_element_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
    ) -> Option<TableElementSnapshot> {
        self.staged_table_elements
            .get(&table_element_key(
                owner_instance,
                table_index,
                element_index,
            ))
            .copied()
    }

    pub(crate) fn original_table_elements(
        &self,
    ) -> Vec<(Option<InstanceId>, u32, u64, TableElementSnapshot)> {
        self.original_table_elements
            .iter()
            .map(|(key, &value)| {
                (
                    granule_owner_instance(key.instance),
                    key.table_index,
                    key.element_index,
                    value,
                )
            })
            .collect()
    }

    pub(crate) fn stage_table_size_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        new_elements: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        self.acquire_table_size_write_owned(owner_instance, table_index, 0)?;
        Ok(self
            .staged_table_sizes
            .insert(
                table_size_granule_id(owner_instance, table_index),
                new_elements,
            )
            .is_none())
    }

    pub(crate) fn staged_table_size_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> Option<u64> {
        self.staged_table_sizes
            .get(&table_size_granule_id(owner_instance, table_index))
            .copied()
    }

    pub(crate) fn stage_memory_granule(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = memory_granule_id_from_u64(None, memory_index, granule_index);
        self.acquire_granule_write(key, 0)?;
        Ok(self.staged_granules.insert(key, bytes).is_none())
    }

    pub(crate) fn stage_memory_write(
        &mut self,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing: &[u8],
    ) -> Result<()> {
        self.stage_memory_write_owned(None, memory_index, addr, bytes, backing)
    }

    pub(crate) fn stage_memory_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing: &[u8],
    ) -> Result<()> {
        self.stage_memory_write_owned_from_backing(
            owner_instance,
            memory_index,
            addr,
            bytes,
            0,
            backing,
            backing.len(),
        )
    }

    pub(crate) fn stage_memory_write_owned_from_backing(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<()> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, bytes.len(), memory_len)?;
        let backing_base = usize::try_from(backing_base)
            .context("tmemory backing base does not fit host usize")?;
        let backing_end = backing_base
            .checked_add(backing.len())
            .context("tmemory backing window overflow")?;
        let mut offset: usize = 0;
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            ensure!(
                granule_range.start >= backing_base && granule_range.end <= backing_end,
                "tmemory backing window does not cover touched granule"
            );
            let local_granule_range =
                (granule_range.start - backing_base)..(granule_range.end - backing_base);
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            self.acquire_granule_write(key, 0)?;
            {
                let staged = self
                    .staged_granules
                    .entry(key)
                    .or_insert_with(|| backing[local_granule_range.clone()].to_vec());
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                staged[granule_offset..granule_chunk_end]
                    .copy_from_slice(&bytes[offset..write_chunk_end]);
            }

            current += chunk_len;
            offset = write_chunk_end;
        }

        Ok(())
    }

    pub(crate) fn stage_tmemory_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        tmemory: &TMemory,
    ) -> Result<()> {
        let snapshot = collect_tmemory_access_snapshot(tmemory, addr, bytes.len())?;
        self.stage_tmemory_write_owned_from_snapshot(
            owner_instance,
            memory_index,
            addr,
            bytes,
            &snapshot,
        )
    }

    pub(crate) fn stage_tmemory_write_owned_from_snapshot(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<()> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, bytes.len(), snapshot.byte_len)?;
        let mut offset: usize = 0;
        let mut current = range.start;

        for granule in &snapshot.granules {
            if current >= range.end {
                break;
            }

            let key = memory_granule_key(owner_instance, memory_index, granule.granule_index)?;
            let granule_offset = current - granule.range.start;
            let chunk_len = (range.end - current).min(granule.range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule write offset overflow")?;
            let write_chunk_end = offset
                .checked_add(chunk_len)
                .context("tmemory write offset overflow")?;

            self.acquire_granule_write(key, granule.version)?;
            {
                let staged = self
                    .staged_granules
                    .entry(key)
                    .or_insert_with(|| granule.bytes.clone());
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                staged[granule_offset..granule_chunk_end]
                    .copy_from_slice(&bytes[offset..write_chunk_end]);
            }

            current += chunk_len;
            offset = write_chunk_end;
        }

        Ok(())
    }

    pub(crate) fn read_memory_overlay(
        &mut self,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing: &[u8],
    ) -> Result<Vec<u8>> {
        self.read_memory_overlay_owned(None, memory_index, addr, len, backing)
    }

    pub(crate) fn read_memory_overlay_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing: &[u8],
    ) -> Result<Vec<u8>> {
        self.read_memory_overlay_owned_from_backing(
            owner_instance,
            memory_index,
            addr,
            len,
            0,
            backing,
            backing.len(),
        )
    }

    pub(crate) fn read_memory_overlay_owned_from_backing(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<Vec<u8>> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, len, memory_len)?;
        let bytes = self.merge_staged_tmemory_range(
            granule_instance(owner_instance),
            memory_index,
            addr,
            len,
            backing_base,
            backing,
            memory_len,
        )?;
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            let chunk_len = (range.end - current).min(granule_range.end - current);

            self.acquire_granule_read(key, 0)?;
            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn read_tmemory_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        tmemory: &TMemory,
    ) -> Result<Vec<u8>> {
        let snapshot = collect_tmemory_access_snapshot(tmemory, addr, len)?;
        self.read_tmemory_owned_from_snapshot(owner_instance, memory_index, addr, len, &snapshot)
    }

    pub(crate) fn read_tmemory_owned_from_snapshot(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<Vec<u8>> {
        self.ensure_active()?;
        let range = checked_tmemory_range(addr, len, snapshot.byte_len)?;
        let bytes = self.merge_staged_tmemory_range(
            granule_instance(owner_instance),
            memory_index,
            addr,
            len,
            snapshot.base,
            &snapshot.bytes,
            snapshot.byte_len,
        )?;
        let mut current = range.start;

        for granule in &snapshot.granules {
            if current >= range.end {
                break;
            }

            let key = memory_granule_key(owner_instance, memory_index, granule.granule_index)?;
            let chunk_len = (range.end - current).min(granule.range.end - current);

            self.acquire_granule_read(key, granule.version)?;
            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn set_scratch(&mut self, bytes: Vec<u8>) -> *mut u8 {
        debug_assert!(self.pending_memory_store.is_none());
        self.scratch = bytes;
        self.scratch.as_mut_ptr()
    }

    pub(crate) fn set_memory_store_scratch(
        &mut self,
        instance: InstanceId,
        memory_index: u32,
        addr: u64,
        bytes: Vec<u8>,
    ) -> Result<*mut u8> {
        self.ensure_active()?;
        let len = bytes.len();
        self.acquire_memory_write_ownership_range_owned(Some(instance), memory_index, addr, len)?;
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            memory_index,
            addr,
            len,
        });
        Ok(self.scratch.as_mut_ptr())
    }

    pub(crate) fn set_tmemory_store_scratch(
        &mut self,
        instance: InstanceId,
        memory_index: u32,
        addr: u64,
        bytes: Vec<u8>,
    ) -> Result<*mut u8> {
        self.ensure_active()?;
        debug_assert!(self.pending_memory_store.is_none());
        self.scratch = bytes;
        self.pending_memory_store = Some(PendingMemoryStore {
            instance,
            memory_index,
            addr,
            len: self.scratch.len(),
        });
        Ok(self.scratch.as_mut_ptr())
    }

    pub(crate) fn pending_memory_store(&self) -> Option<(InstanceId, u32, u64, usize)> {
        self.pending_memory_store.map(|pending| {
            (
                pending.instance,
                pending.memory_index,
                pending.addr,
                pending.len,
            )
        })
    }

    pub(crate) fn flush_memory_store_scratch(&mut self, backing: &[u8]) -> Result<bool> {
        self.flush_memory_store_scratch_from_backing(0, backing, backing.len())
    }

    pub(crate) fn flush_memory_store_scratch_from_backing(
        &mut self,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<bool> {
        self.ensure_active()?;
        let Some(pending) = self.pending_memory_store.take() else {
            return Ok(false);
        };
        ensure!(
            self.scratch.len() == pending.len,
            "pending tmemory store scratch length mismatch"
        );
        let bytes = self.scratch[..pending.len].to_vec();
        self.stage_memory_write_owned_from_backing(
            Some(pending.instance),
            pending.memory_index,
            pending.addr,
            &bytes,
            backing_base,
            backing,
            memory_len,
        )?;
        Ok(true)
    }

    pub(crate) fn flush_tmemory_store_scratch_from_snapshot(
        &mut self,
        snapshot: &TMemoryAccessSnapshot,
    ) -> Result<bool> {
        self.ensure_active()?;
        let Some(pending) = self.pending_memory_store.take() else {
            return Ok(false);
        };
        ensure!(
            self.scratch.len() == pending.len,
            "pending tmemory store scratch length mismatch"
        );
        let bytes = self.scratch[..pending.len].to_vec();
        self.stage_tmemory_write_owned_from_snapshot(
            Some(pending.instance),
            pending.memory_index,
            pending.addr,
            &bytes,
            snapshot,
        )?;
        Ok(true)
    }

    pub(crate) fn commit_tmemory_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        tmemory: &mut TMemory,
    ) -> Result<bool> {
        self.ensure_active()?;
        let staged = self.staged_tmemory_granules_owned(owner_instance, memory_index)?;
        if staged.is_empty() {
            return Ok(false);
        }

        let stream_id = u32::try_from(self.active_transaction_required_raw()?)
            .context("transaction id does not fit durable tmemory stream id")?;
        let staged = staged
            .iter()
            .map(|(_, granule_index, bytes)| {
                Ok((
                    u64::try_from(*granule_index)
                        .context("tmemory granule index does not fit durable log")?,
                    bytes.clone(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        if tmemory.backend() == TMemoryBackend::VMemory {
            tmemory.commit_staged_tmemory_granules_direct(&staged)?;
        } else {
            let mut final_marker = None;
            for (granule_index, bytes) in &staged {
                let undo = tmemory.prepare_tmemory_undo_record(
                    owner_instance.map(InstanceId::as_u32),
                    memory_index,
                    *granule_index,
                    bytes,
                )?;
                final_marker = Some(
                    self.publish_tmemory_undo_before_in_place_write(stream_id, stream_id, &undo)?,
                );
            }
            for (granule_index, bytes) in &staged {
                tmemory.commit_staged_tmemory_granule(*granule_index, bytes)?;
            }
            if let Some(marker) = final_marker {
                self.publish_commit_lp(stream_id, stream_id, marker)?;
            }
        }

        self.remove_staged_tmemory_granules_owned(owner_instance, memory_index)?;
        Ok(true)
    }

    pub(crate) fn staged_memory_granule(
        &self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&[u8]> {
        self.staged_granules
            .get(&memory_granule_id_from_u64(
                None,
                memory_index,
                granule_index,
            ))
            .map(Vec::as_slice)
    }

    pub(crate) fn staged_memory_granule_mut(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Option<&mut [u8]> {
        self.staged_granules
            .get_mut(&memory_granule_id_from_u64(
                None,
                memory_index,
                granule_index,
            ))
            .map(Vec::as_mut_slice)
    }

    pub(crate) fn acquire_memory_granule_read(
        &mut self,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<bool> {
        self.acquire_memory_granule_read_owned(None, memory_index, granule_index)
    }

    pub(crate) fn acquire_memory_granule_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> Result<bool> {
        self.acquire_granule_read(
            memory_granule_id_from_u64(owner_instance, memory_index, granule_index),
            0,
        )
    }

    pub(crate) fn acquire_memory_size_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_read(memory_size_granule_id(owner_instance, memory_index), 0)
    }

    pub(crate) fn acquire_memory_size_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_write(memory_size_granule_id(owner_instance, memory_index), 0)
    }

    pub(crate) fn acquire_global_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_read(global_granule_id(owner_instance, global_index), 0)
    }

    pub(crate) fn acquire_global_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> Result<bool> {
        self.acquire_granule_write(global_granule_id(owner_instance, global_index), 0)
    }

    pub(crate) fn acquire_object_read(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<bool> {
        self.acquire_granule_read(
            object_table.granule_id(object_id)?,
            object_table.version(object_id)?,
        )
    }

    pub(crate) fn acquire_object_write(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<bool> {
        self.acquire_granule_write(
            object_table.granule_id(object_id)?,
            object_table.version(object_id)?,
        )
    }

    pub(crate) fn acquire_tref_read_for_gc_ref(
        &mut self,
        object_table: &ObjectTable,
        gc_ref: u32,
    ) -> Result<bool> {
        let Some(object_id) = object_table.known_persistent_object_id_for_gc_ref(gc_ref)? else {
            return Ok(false);
        };
        self.acquire_object_read(object_table, object_id)
    }

    pub(crate) fn acquire_tref_write_for_gc_ref(
        &mut self,
        object_table: &ObjectTable,
        gc_ref: u32,
    ) -> Result<bool> {
        let Some(object_id) = object_table.known_persistent_object_id_for_gc_ref(gc_ref)? else {
            return Ok(false);
        };
        self.acquire_object_write(object_table, object_id)
    }

    pub(crate) fn read_object_payload(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<ObjectPayload> {
        if object_table.is_persistent(object_id)? {
            let granule = object_table.granule_id(object_id)?;
            ensure!(
                self.owns_granule_read(granule),
                "transactional object read permission was not acquired"
            );
        }
        if let Some(payload) = self.staged_objects.get(&object_id) {
            return Ok(payload.clone());
        }
        object_table.payload(object_id)
    }

    pub(crate) fn stage_object_payload(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        payload: ObjectPayload,
    ) -> Result<bool> {
        ensure!(
            object_table.kind(object_id)? == payload.kind(),
            "object payload kind does not match object table slot kind"
        );
        if object_table.is_persistent(object_id)? {
            let granule = object_table.granule_id(object_id)?;
            ensure!(
                self.owns_granule_write(granule),
                "transactional object write permission was not acquired"
            );
        }
        Ok(self.staged_objects.insert(object_id, payload).is_none())
    }

    pub(crate) fn read_struct_field(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        field_index: usize,
    ) -> Result<ObjectValue> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Struct(fields) = payload else {
            bail!("object is not a struct");
        };
        fields
            .get(field_index)
            .cloned()
            .context("out of bounds struct access")
    }

    pub(crate) fn stage_struct_field(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        field_index: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Struct(mut fields) = payload else {
            bail!("object is not a struct");
        };
        let field = fields
            .get_mut(field_index)
            .context("out of bounds struct access")?;
        *field = value;
        self.stage_object_payload(object_table, object_id, ObjectPayload::Struct(fields))?;
        Ok(())
    }

    pub(crate) fn read_array_len(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
    ) -> Result<usize> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(elements) = payload else {
            bail!("object is not an array");
        };
        Ok(elements.len())
    }

    pub(crate) fn read_array_element(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        element_index: usize,
    ) -> Result<ObjectValue> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(elements) = payload else {
            bail!("object is not an array");
        };
        elements
            .get(element_index)
            .cloned()
            .context("out of bounds array access")
    }

    pub(crate) fn stage_array_element(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        element_index: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let element = elements
            .get_mut(element_index)
            .context("out of bounds array access")?;
        *element = value;
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn fill_array_range(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        start: usize,
        len: usize,
        value: ObjectValue,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let end = checked_array_range_end(start, len)?;
        ensure!(end <= elements.len(), "out of bounds array access");
        elements[start..end].fill(value);
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn write_array_range(
        &mut self,
        object_table: &ObjectTable,
        object_id: ObjectId,
        start: usize,
        values: Vec<ObjectValue>,
    ) -> Result<()> {
        let payload = self.read_object_payload(object_table, object_id)?;
        let ObjectPayload::Array(mut elements) = payload else {
            bail!("object is not an array");
        };
        let end = checked_array_range_end(start, values.len())?;
        ensure!(end <= elements.len(), "out of bounds array access");
        elements[start..end].clone_from_slice(&values);
        self.stage_object_payload(object_table, object_id, ObjectPayload::Array(elements))?;
        Ok(())
    }

    pub(crate) fn copy_array_range(
        &mut self,
        object_table: &ObjectTable,
        dst_object_id: ObjectId,
        dst_start: usize,
        src_object_id: ObjectId,
        src_start: usize,
        len: usize,
    ) -> Result<()> {
        let src_payload = self.read_object_payload(object_table, src_object_id)?;
        let ObjectPayload::Array(src_elements) = src_payload else {
            bail!("source object is not an array");
        };
        let src_end = checked_array_range_end(src_start, len)?;
        ensure!(src_end <= src_elements.len(), "out of bounds array access");
        let copied = src_elements[src_start..src_end].to_vec();

        let dst_payload = self.read_object_payload(object_table, dst_object_id)?;
        let ObjectPayload::Array(mut dst_elements) = dst_payload else {
            bail!("destination object is not an array");
        };
        let dst_end = checked_array_range_end(dst_start, len)?;
        ensure!(dst_end <= dst_elements.len(), "out of bounds array access");
        dst_elements[dst_start..dst_end].clone_from_slice(&copied);
        self.stage_object_payload(
            object_table,
            dst_object_id,
            ObjectPayload::Array(dst_elements),
        )?;
        Ok(())
    }

    pub(crate) fn create_i31(&self, value: i32) -> Result<I31Value> {
        self.ensure_active()?;
        Ok(I31Value::new(value))
    }

    pub(crate) fn promote_extern_ref_for_persistence(
        &mut self,
        _extern_ref: u64,
    ) -> Result<ObjectId> {
        self.ensure_active()?;
        self.abort()?;
        bail!("persistent extern object promotion is not supported")
    }

    pub(crate) fn commit_object_payloads(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<bool> {
        self.commit_object_payloads_with(object_table, |_| Ok(()))
    }

    pub(crate) fn commit_object_payloads_into(
        &mut self,
        object_table: &mut ObjectTable,
        pending_publications: &mut Vec<persist::PendingPublication>,
    ) -> Result<bool> {
        self.commit_object_payloads_with(object_table, |publication| {
            pending_publications.push(publication);
            Ok(())
        })
    }

    fn commit_object_payloads_with<F>(
        &mut self,
        object_table: &mut ObjectTable,
        mut publish: F,
    ) -> Result<bool>
    where
        F: FnMut(persist::PendingPublication) -> Result<()>,
    {
        self.ensure_active()?;
        self.validate_active_object_reads(object_table)?;
        if self.staged_objects.is_empty() {
            return Ok(false);
        }
        let updates = self
            .staged_objects
            .iter()
            .map(|(&object_id, payload)| {
                ensure!(
                    object_table.kind(object_id)? == payload.kind(),
                    "object payload kind does not match object table slot kind"
                );
                Ok((object_id, payload.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        for (object_id, payload) in updates {
            object_table.update_payload(object_id, payload)?;
            if object_table.is_persistent(object_id)? {
                publish(object_table.object_pending_publication(object_id)?)?;
            }
        }
        self.staged_objects.clear();
        Ok(true)
    }

    pub(crate) fn validate_active_object_reads(&self, object_table: &ObjectTable) -> Result<()> {
        for granule in self.active_read_granules()? {
            let Some(object_id) = object_granule_object_id(granule) else {
                continue;
            };
            self.validate_active_read(granule, object_table.version(object_id)?)?;
        }
        Ok(())
    }

    pub(crate) fn owns_memory_size_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> bool {
        self.owns_granule_read(memory_size_granule_id(owner_instance, memory_index))
    }

    pub(crate) fn owns_memory_size_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> bool {
        self.owns_granule_write(memory_size_granule_id(owner_instance, memory_index))
    }

    pub(crate) fn owns_global_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> bool {
        self.owns_granule_read(global_granule_id(owner_instance, global_index))
    }

    pub(crate) fn owns_global_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        global_index: u32,
    ) -> bool {
        self.owns_granule_write(global_granule_id(owner_instance, global_index))
    }

    pub(crate) fn owns_struct_read(&self, object_id: ObjectId) -> bool {
        self.owns_granule_read(GranuleId::TStruct { object_id })
    }

    pub(crate) fn owns_struct_write(&self, object_id: ObjectId) -> bool {
        self.owns_granule_write(GranuleId::TStruct { object_id })
    }

    pub(crate) fn owns_array_read(&self, object_id: ObjectId) -> bool {
        self.owns_granule_read(GranuleId::TArray { object_id })
    }

    pub(crate) fn owns_array_write(&self, object_id: ObjectId) -> bool {
        self.owns_granule_write(GranuleId::TArray { object_id })
    }

    pub(crate) fn acquire_memory_granule_write(
        &mut self,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.acquire_memory_granule_write_owned(None, memory_index, granule_index, bytes)
    }

    pub(crate) fn acquire_memory_granule_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
        bytes: Vec<u8>,
    ) -> Result<bool> {
        self.ensure_active()?;
        let key = memory_granule_id_from_u64(owner_instance, memory_index, granule_index);
        self.acquire_granule_write(key, 0)?;
        Ok(self.staged_granules.insert(key, bytes).is_none())
    }

    fn acquire_memory_write_ownership_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        addr: u64,
        len: usize,
    ) -> Result<()> {
        let start = usize::try_from(addr).context("tmemory address does not fit host usize")?;
        let end = start
            .checked_add(len)
            .context("tmemory pending store range overflow")?;
        let mut current = start;

        while current < end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_end = granule_index
                .checked_add(1)
                .and_then(|index| index.checked_mul(TMEMORY_GRANULE_SIZE))
                .context("tmemory pending store granule overflow")?;
            let key = memory_granule_key(owner_instance, memory_index, granule_index)?;
            self.acquire_granule_write(key, 0)?;
            current = end.min(granule_end);
        }

        Ok(())
    }

    pub(crate) fn owns_memory_granule_read(&self, memory_index: u32, granule_index: u64) -> bool {
        self.owns_memory_granule_read_owned(None, memory_index, granule_index)
    }

    pub(crate) fn owns_memory_granule_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> bool {
        self.owns_granule_read(memory_granule_id_from_u64(
            owner_instance,
            memory_index,
            granule_index,
        ))
    }

    pub(crate) fn owns_memory_granule_write(&self, memory_index: u32, granule_index: u64) -> bool {
        self.owns_memory_granule_write_owned(None, memory_index, granule_index)
    }

    pub(crate) fn owns_memory_granule_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
        granule_index: u64,
    ) -> bool {
        self.owns_granule_write(memory_granule_id_from_u64(
            owner_instance,
            memory_index,
            granule_index,
        ))
    }

    pub(crate) fn acquire_table_granule_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.acquire_granule_read(key, current_version)
    }

    pub(crate) fn acquire_table_granule_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        element_index: u64,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_granule_id(owner_instance, table_index, element_index);
        self.acquire_granule_write(key, current_version)
    }

    pub(crate) fn acquire_table_granule_read_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        start_element: u64,
        len: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        if len == 0 {
            return Ok(false);
        }
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable read range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            inserted |= self.acquire_granule_read(key, current_version)?;
        }
        Ok(inserted)
    }

    pub(crate) fn acquire_table_granule_write_range_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        start_element: u64,
        len: u64,
        current_version: u64,
    ) -> Result<bool> {
        self.ensure_active()?;
        if len == 0 {
            return Ok(false);
        }
        let last_element = start_element
            .checked_add(len - 1)
            .context("ttable write range overflow")?;
        let first_granule = start_element >> TTABLE_GRANULE_SHIFT;
        let last_granule = last_element >> TTABLE_GRANULE_SHIFT;
        let mut inserted = false;
        for granule_index in first_granule..=last_granule {
            let key = table_granule_id_from_u64(owner_instance, table_index, granule_index);
            inserted |= self.acquire_granule_write(key, current_version)?;
        }
        Ok(inserted)
    }

    pub(crate) fn acquire_table_size_read_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_size_granule_id(owner_instance, table_index);
        self.acquire_granule_read(key, current_version)
    }

    pub(crate) fn acquire_table_size_write_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        current_version: u64,
    ) -> Result<bool> {
        let key = table_size_granule_id(owner_instance, table_index);
        self.acquire_granule_write(key, current_version)
    }

    pub(crate) fn owns_table_granule_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        granule_index: u64,
    ) -> bool {
        self.owns_granule_read(table_granule_id_from_u64(
            owner_instance,
            table_index,
            granule_index,
        ))
    }

    pub(crate) fn owns_table_granule_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
        granule_index: u64,
    ) -> bool {
        self.owns_granule_write(table_granule_id_from_u64(
            owner_instance,
            table_index,
            granule_index,
        ))
    }

    pub(crate) fn owns_table_size_read_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> bool {
        self.owns_granule_read(table_size_granule_id(owner_instance, table_index))
    }

    pub(crate) fn owns_table_size_write_owned(
        &self,
        owner_instance: Option<InstanceId>,
        table_index: u32,
    ) -> bool {
        self.owns_granule_write(table_size_granule_id(owner_instance, table_index))
    }

    fn merge_staged_tmemory_range(
        &self,
        instance: Option<u32>,
        memory_index: u32,
        addr: u64,
        len: usize,
        backing_base: u64,
        backing: &[u8],
        memory_len: usize,
    ) -> Result<Vec<u8>> {
        let range = checked_tmemory_range(addr, len, memory_len)?;
        let backing_base = usize::try_from(backing_base)
            .context("tmemory backing base does not fit host usize")?;
        let backing_end = backing_base
            .checked_add(backing.len())
            .context("tmemory backing window overflow")?;
        ensure!(
            range.start >= backing_base && range.end <= backing_end,
            "tmemory backing window does not cover read range"
        );

        let mut bytes = backing[range.start - backing_base..range.end - backing_base].to_vec();
        let mut current = range.start;

        while current < range.end {
            let granule_index = current / TMEMORY_GRANULE_SIZE;
            let granule_range = tmemory_granule_backing_range(granule_index, memory_len)?;
            let key = memory_granule_id_for_instance(instance, memory_index, granule_index)?;
            let granule_offset = current - granule_range.start;
            let chunk_len = (range.end - current).min(granule_range.end - current);
            let granule_chunk_end = granule_offset
                .checked_add(chunk_len)
                .context("tmemory granule read offset overflow")?;

            if let Some(staged) = self.staged_granules.get(&key) {
                ensure!(
                    granule_chunk_end <= staged.len(),
                    "staged tmemory granule length mismatch"
                );
                let read_offset = current - range.start;
                let read_chunk_end = read_offset
                    .checked_add(chunk_len)
                    .context("tmemory read offset overflow")?;
                bytes[read_offset..read_chunk_end]
                    .copy_from_slice(&staged[granule_offset..granule_chunk_end]);
            }

            current += chunk_len;
        }

        Ok(bytes)
    }

    pub(crate) fn staged_tmemory_granules_owned(
        &self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<Vec<(GranuleId, usize, Vec<u8>)>> {
        self.ensure_active()?;
        let owner = granule_instance(owner_instance);
        let mut staged = Vec::new();
        for (key, bytes) in &self.staged_granules {
            let GranuleId::TMemory {
                instance,
                memory_index: staged_memory_index,
                granule_index,
            } = *key
            else {
                continue;
            };
            if instance != owner || staged_memory_index != memory_index {
                continue;
            }
            staged.push((
                key.to_owned(),
                usize::try_from(granule_index)
                    .context("tmemory granule index does not fit host usize")?,
                bytes.clone(),
            ));
        }
        Ok(staged)
    }

    pub(crate) fn remove_staged_tmemory_granules_owned(
        &mut self,
        owner_instance: Option<InstanceId>,
        memory_index: u32,
    ) -> Result<()> {
        self.ensure_active()?;
        let owner = granule_instance(owner_instance);
        let keys = self
            .staged_granules
            .keys()
            .filter_map(|key| match *key {
                GranuleId::TMemory {
                    instance,
                    memory_index: staged_memory_index,
                    ..
                } if instance == owner && staged_memory_index == memory_index => Some(*key),
                _ => None,
            })
            .collect::<Vec<_>>();
        for key in keys {
            self.staged_granules.remove(&key);
        }
        Ok(())
    }

    fn active_transaction_required(&self) -> Result<TransactionId> {
        ensure!(
            current_thread_transaction() == self.active,
            "thread transaction state does not match active transaction"
        );
        self.active_transaction()
            .context("transaction operation requires an active transaction")
    }

    fn ensure_active(&self) -> Result<()> {
        ensure!(
            self.active.is_some(),
            "transaction operation requires an active transaction"
        );
        ensure!(
            current_thread_transaction() == self.active,
            "thread transaction state does not match active transaction"
        );
        Ok(())
    }

    fn suspend_active_workspace(&mut self) -> Result<Option<TransactionId>> {
        let Some(transaction) = self.active.take() else {
            return Ok(None);
        };
        let workspace = self.take_workspace();
        ensure!(
            self.suspended.insert(transaction, workspace).is_none(),
            "transaction workspace is already suspended"
        );
        Ok(Some(transaction))
    }

    fn take_workspace(&mut self) -> TransactionWorkspace {
        TransactionWorkspace {
            staged_globals: mem::take(&mut self.staged_globals),
            staged_granules: mem::take(&mut self.staged_granules),
            staged_memory_sizes: mem::take(&mut self.staged_memory_sizes),
            staged_table_sizes: mem::take(&mut self.staged_table_sizes),
            staged_table_elements: mem::take(&mut self.staged_table_elements),
            original_table_elements: mem::take(&mut self.original_table_elements),
            staged_objects: mem::take(&mut self.staged_objects),
            allocated_objects: mem::take(&mut self.allocated_objects),
            read_granules: mem::take(&mut self.read_granules),
            write_granules: mem::take(&mut self.write_granules),
            scratch: mem::take(&mut self.scratch),
            pending_memory_store: self.pending_memory_store.take(),
        }
    }

    fn install_workspace(&mut self, workspace: TransactionWorkspace) {
        self.staged_globals = workspace.staged_globals;
        self.staged_granules = workspace.staged_granules;
        self.staged_memory_sizes = workspace.staged_memory_sizes;
        self.staged_table_sizes = workspace.staged_table_sizes;
        self.staged_table_elements = workspace.staged_table_elements;
        self.original_table_elements = workspace.original_table_elements;
        self.staged_objects = workspace.staged_objects;
        self.allocated_objects = workspace.allocated_objects;
        self.read_granules = workspace.read_granules;
        self.write_granules = workspace.write_granules;
        self.scratch = workspace.scratch;
        self.pending_memory_store = workspace.pending_memory_store;
    }

    fn clear_active(&mut self) {
        if let Some(transaction) = self.active.take() {
            self.locks.release_transaction(transaction);
        }
        replace_current_thread_transaction(None);
        self.install_workspace(TransactionWorkspace::default());
    }
}

#[cfg(test)]
impl TransactionState {
    fn new_for_test(transaction: TransactionId) -> Self {
        replace_current_thread_transaction(Some(transaction));
        Self {
            active: Some(transaction),
            next_id: transaction.as_raw().saturating_add(1),
            ..Self::default()
        }
    }

    fn new_for_test_with_durable_log(
        transaction: TransactionId,
        durable_log: TxDurableLog,
    ) -> Self {
        let mut state = Self::new_for_test(transaction);
        state.durable_log = durable_log;
        state
    }

    fn stage_granule_for_test(&mut self, granule: GranuleId, bytes: Vec<u8>) {
        self.staged_granules.insert(granule, bytes);
    }

    fn read_tmemory_range_for_test(
        &self,
        instance: Option<u32>,
        memory_index: u32,
        addr: u64,
        len: usize,
        committed: &[u8],
    ) -> Result<Vec<u8>> {
        self.merge_staged_tmemory_range(
            instance,
            memory_index,
            addr,
            len,
            0,
            committed,
            committed.len(),
        )
    }

    fn stage_tmemory_write_for_test(
        &mut self,
        instance: u32,
        memory_index: u32,
        addr: u64,
        bytes: &[u8],
        tmemory: &TMemory,
    ) -> Result<()> {
        self.stage_tmemory_write_owned(
            Some(InstanceId::from_u32(instance)),
            memory_index,
            addr,
            bytes,
            tmemory,
        )
    }

    fn commit_tmemory_for_test(&mut self, tmemory: &mut TMemory) -> Result<bool> {
        self.commit_tmemory_owned(Some(InstanceId::from_u32(0)), 0, tmemory)
    }
}

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

fn object_slot_index(object_id: ObjectId) -> Result<usize> {
    usize::try_from(object_id.object_index).context("object id does not fit host usize")
}

fn object_granule_object_id(granule: GranuleId) -> Option<ObjectId> {
    match granule {
        GranuleId::TStruct { object_id } | GranuleId::TArray { object_id } => Some(object_id),
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
mod tests {
    use super::*;

    fn with_transaction_memory_metadata(wasm: &[u8]) -> Vec<u8> {
        with_transaction_object_metadata(wasm, &[1, 1, 0, 0])
    }

    fn with_transaction_global_metadata(wasm: &[u8]) -> Vec<u8> {
        with_transaction_object_metadata(wasm, &[1, 0, 1, 0])
    }

    fn with_transaction_object_metadata(wasm: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut wasm = wasm.to_vec();
        wasm.extend_from_slice(&[
            0x00, 0x20, 0x1b, b's', b'h', b'i', b's', b'o', b'f', b't', b'.', b't', b'r', b'a',
            b'n', b's', b'a', b'c', b't', b'i', b'o', b'n', b'.', b'o', b'b', b'j', b'e', b'c',
            b't', b's',
        ]);
        wasm.extend_from_slice(payload);
        wasm
    }

    fn transaction_test_module(engine: &crate::Engine, wat: &str) -> crate::Module {
        crate::Module::new(engine, wat::parse_str(wat).unwrap()).unwrap()
    }

    #[test]
    fn mock_transaction_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_simd_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore))
              (tfunc (export "read_lane0") (result i32)
                (i32.const 0)
                (v128.tload)
                (i32x4.extract_lane 0)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_lane0 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane0")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_lane0.call(&mut store, ()).unwrap(), 287454020);
    }

    #[test]
    fn mock_transaction_simd_lane_store_commits_to_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write_lane")
                (i32.const 0)
                (v128.const i32x4 287454020 1432778632 16909060 84281096)
                (v128.tstore32_lane 1))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_lane = instance
            .get_typed_func::<(), ()>(&mut store, "write_lane")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_lane.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 1432778632);
    }

    #[test]
    fn mock_transaction_tfunc_store_commits_on_return() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 42)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_nested_tfunc_reuses_active_transaction_until_outer_return() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc $inner
                (i32.tstore (i32.const 0) (i32.const 1)))
              (tfunc (export "outer_fail")
                (call $inner)
                (i32.tstore (i32.const 0) (i32.const 2))
                (tfail))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let outer_fail = instance
            .get_typed_func::<(), ()>(&mut store, "outer_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        outer_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_fail_discards_memory_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_global_i32_set_commits_to_backing_global() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i32) (i32.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 42)))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 42);
    }

    #[test]
    fn mock_transaction_global_i64_fail_discards_staged_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i64) (i64.const 7))
              (func (export "write_fail")
                (ttry)
                (tglobal.set $g (i64.const 99))
                (tfail))
              (func (export "read") (result i64)
                (global.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i64>(&mut store, "read")
            .unwrap();

        write_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 7);
    }

    #[test]
    fn mock_transaction_global_get_observes_staged_i32_write() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut i32) (i32.const 1))
              (func (export "write_read") (result i32)
                (ttry)
                (tglobal.set $g (i32.const 77))
                (tglobal.get $g)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_read = instance
            .get_typed_func::<(), i32>(&mut store, "write_read")
            .unwrap();

        assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
    }

    #[test]
    fn mock_transaction_global_imported_tglobal_commits_to_backing_global() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (export "g") (mut i32) (i32.const 5))
              (func (export "read") (result i32)
                (global.get $g)))
            "#,
        );
        let consumer = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "g" (tglobal $g (mut i32)))
              (func (export "write")
                (ttry)
                (tglobal.set $g (i32.const 11))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let global = provider.get_global(&mut store, "g").unwrap();
        let consumer = crate::Instance::new(&mut store, &consumer, &[global.into()]).unwrap();
        let write = consumer
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = provider
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 11);
    }

    #[test]
    fn mock_transaction_global_float_sets_commit_bitwise() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $f32 (mut f32) (f32.const 0))
              (tglobal $f64 (mut f64) (f64.const 0))
              (func (export "write")
                (ttry)
                (tglobal.set $f32 (f32.const -13.5))
                (tglobal.set $f64 (f64.const 42.25)))
              (func (export "read_f32_bits") (result i32)
                (i32.reinterpret_f32 (global.get $f32)))
              (func (export "read_f64_bits") (result i64)
                (i64.reinterpret_f64 (global.get $f64))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_f32_bits = instance
            .get_typed_func::<(), i32>(&mut store, "read_f32_bits")
            .unwrap();
        let read_f64_bits = instance
            .get_typed_func::<(), i64>(&mut store, "read_f64_bits")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(
            read_f32_bits.call(&mut store, ()).unwrap() as u32,
            (-13.5f32).to_bits()
        );
        assert_eq!(
            read_f64_bits.call(&mut store, ()).unwrap() as u64,
            42.25f64.to_bits()
        );
    }

    #[test]
    fn mock_transaction_global_v128_get_reads_defined_global() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 287454020 1432778632 16909060 84281096))
              (tfunc (export "read_lane1") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 1)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let read_lane1 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane1")
            .unwrap();

        assert_eq!(read_lane1.call(&mut store, ()).unwrap(), 1432778632);
    }

    #[test]
    fn mock_transaction_global_v128_set_commits_bitwise() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tglobal $g (mut v128) (v128.const i32x4 0 0 0 0))
              (tfunc (export "write")
                (tglobal.set $g (v128.const i32x4 287454020 1432778632 16909060 84281096)))
              (tfunc (export "read_lane2") (result i32)
                (tglobal.get $g)
                (i32x4.extract_lane 2)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_lane2 = instance
            .get_typed_func::<(), i32>(&mut store, "read_lane2")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_lane2.call(&mut store, ()).unwrap(), 16909060);
    }

    #[test]
    fn mock_transaction_plain_func_memory_size_requires_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 2)
              (func (export "size") (result i32)
                (tmemory.size)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();

        let error = size.call(&mut store, ()).unwrap_err();

        assert!(
            format!("{error:?}").contains("transaction operation requires an active transaction")
        );
    }

    #[test]
    fn mock_transaction_tfunc_size_and_grow_commit_against_tmemory() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 0 1)
              (tfunc (export "size") (result i32)
                (tmemory.size))
              (tfunc (export "grow") (param i32) (result i32)
                (tmemory.grow (local.get 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();
        let grow = instance
            .get_typed_func::<i32, i32>(&mut store, "grow")
            .unwrap();

        assert_eq!(size.call(&mut store, ()).unwrap(), 0);
        assert_eq!(grow.call(&mut store, 1).unwrap(), 0);
        assert_eq!(size.call(&mut store, ()).unwrap(), 1);
    }

    #[test]
    fn mock_transaction_ttable_funcref_paths_hit_runtime_libcalls() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (ttable $t 1 funcref)
              (elem declare func $target)
              (func $target)
              (tfunc (export "size") (result i32)
                (ttable.size $t))
              (tfunc (export "grow") (result i32)
                (ref.null func)
                (i32.const 2)
                (ttable.grow $t))
              (tfunc (export "is_null") (result i32)
                (i32.const 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "set")
                (i32.const 0)
                (ref.func $target)
                (ttable.set $t)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let size = instance
            .get_typed_func::<(), i32>(&mut store, "size")
            .unwrap();
        let grow = instance
            .get_typed_func::<(), i32>(&mut store, "grow")
            .unwrap();
        let is_null = instance
            .get_typed_func::<(), i32>(&mut store, "is_null")
            .unwrap();
        let set = instance
            .get_typed_func::<(), ()>(&mut store, "set")
            .unwrap();

        assert_eq!(size.call(&mut store, ()).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, ()).unwrap(), 1);
        set.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, ()).unwrap(), 0);
        assert_eq!(grow.call(&mut store, ()).unwrap(), 1);
        assert_eq!(size.call(&mut store, ()).unwrap(), 3);
    }

    #[test]
    fn mock_transaction_ttable_bulk_funcref_paths_hit_runtime_libcalls() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (table $t 4 tfuncref)
              (elem $e func $target)
              (func $target)
              (tfunc (export "is_null") (param i32) (result i32)
                (local.get 0)
                (ttable.get $t)
                (ref.is_null))
              (tfunc (export "fill")
                (i32.const 0)
                (ref.func $target)
                (i32.const 1)
                (ttable.fill $t))
              (tfunc (export "copy")
                (i32.const 1)
                (i32.const 0)
                (i32.const 1)
                (ttable.copy $t $t))
              (tfunc (export "init")
                (i32.const 2)
                (i32.const 0)
                (i32.const 1)
                (ttable.init $t $e)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let is_null = instance
            .get_typed_func::<i32, i32>(&mut store, "is_null")
            .unwrap();
        let fill = instance
            .get_typed_func::<(), ()>(&mut store, "fill")
            .unwrap();
        let copy = instance
            .get_typed_func::<(), ()>(&mut store, "copy")
            .unwrap();
        let init = instance
            .get_typed_func::<(), ()>(&mut store, "init")
            .unwrap();

        assert_eq!(is_null.call(&mut store, 0).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, 1).unwrap(), 1);
        assert_eq!(is_null.call(&mut store, 2).unwrap(), 1);
        fill.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 0).unwrap(), 0);
        copy.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 1).unwrap(), 0);
        init.call(&mut store, ()).unwrap();
        assert_eq!(is_null.call(&mut store, 2).unwrap(), 0);
    }

    #[test]
    fn module_compilation_rejects_transaction_table_get_on_ordinary_table() {
        let engine = crate::Engine::default();
        let error = crate::Module::new(
            &engine,
            wat::parse_str(
                r#"
                (module
                  (table $t 1 funcref)
                  (func (result i32)
                    (i32.const 0)
                    (ttable.get $t)
                    (ref.is_null)))
                "#,
            )
            .unwrap(),
        )
        .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional table operator requires ttable"),
            "{error}"
        );
    }

    #[test]
    fn mock_transaction_static_tdata_initializes_tmemory_sidecar() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tdata (i32.const 0) "abcdefgh")
              (tfunc (export "read") (result i64)
                (i64.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let read = instance
            .get_typed_func::<(), i64>(&mut store, "read")
            .unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0x6867_6665_6463_6261);
    }

    #[test]
    fn mock_transaction_read_after_write_uses_pending_store_scratch() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func (export "write_read") (result i32)
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 77))
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_read = instance
            .get_typed_func::<(), i32>(&mut store, "write_read")
            .unwrap();

        assert_eq!(write_read.call(&mut store, ()).unwrap(), 77);
    }

    #[test]
    fn mock_transaction_tmemory_trap_clears_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (tfunc (export "trap")
                (i32.tstore (i32.const 0) (i32.const 42))
                (drop (i32.tload (i32.const 65536))))
              (tfunc (export "write")
                (i32.tstore (i32.const 0) (i32.const 7)))
              (tfunc (export "read") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let trap = instance
            .get_typed_func::<(), ()>(&mut store, "trap")
            .unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        assert!(trap.call(&mut store, ()).is_err());
        assert_eq!(read.call(&mut store, ()).unwrap(), 0);

        write.call(&mut store, ()).unwrap();
        assert_eq!(read.call(&mut store, ()).unwrap(), 7);
    }

    #[test]
    fn mock_transaction_plain_wasm_trap_clears_active_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $sig (func))
              (tmemory 1)
              (table 0 funcref)
              (tfunc (export "trap_after_tload")
                (drop (i32.tload (i32.const 0)))
                (call_indirect (type $sig) (i32.const 0)))
              (tfunc (export "recover") (result i32)
                (i32.tload (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let trap_after_tload = instance
            .get_typed_func::<(), ()>(&mut store, "trap_after_tload")
            .unwrap();
        let recover = instance
            .get_typed_func::<(), i32>(&mut store, "recover")
            .unwrap();

        trap_after_tload.call(&mut store, ()).unwrap_err();

        assert_eq!(recover.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_unexecuted_ttry_does_not_commit_caller_transaction() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory 1)
              (func $maybe_begin (param i32)
                (local.get 0)
                (if
                  (then
                    (ttry))))
              (func (export "write_then_fail")
                (ttry)
                (i32.tstore (i32.const 0) (i32.const 42))
                (call $maybe_begin (i32.const 0))
                (tfail))
              (func (export "read") (result i32)
                (i32.load (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let write_then_fail = instance
            .get_typed_func::<(), ()>(&mut store, "write_then_fail")
            .unwrap();
        let read = instance
            .get_typed_func::<(), i32>(&mut store, "read")
            .unwrap();

        write_then_fail.call(&mut store, ()).unwrap();

        assert_eq!(read.call(&mut store, ()).unwrap(), 0);
    }

    #[test]
    fn mock_transaction_store_uses_defined_memory_index_after_import() {
        let engine = crate::Engine::default();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "ordinary" (memory 1))
              (tmemory $tx 1)
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 55)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let ordinary = crate::Memory::new(&mut store, crate::MemoryType::new(1, None)).unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[ordinary.into()]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_tx = instance
            .get_typed_func::<(), i32>(&mut store, "read_tx")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_tx.call(&mut store, ()).unwrap(), 55);
    }

    #[test]
    fn mock_transaction_store_uses_imported_tmemory_vmctx() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
        );
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "tx" (tmemory $tx 1))
              (tfunc (export "write")
                (i32.tstore $tx (i32.const 0) (i32.const 66)))
              (tfunc (export "read_tx") (result i32)
                (i32.tload $tx (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let tx = provider.get_memory(&mut store, "tx").unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
        let write = instance
            .get_typed_func::<(), ()>(&mut store, "write")
            .unwrap();
        let read_tx = instance
            .get_typed_func::<(), i32>(&mut store, "read_tx")
            .unwrap();

        write.call(&mut store, ()).unwrap();

        assert_eq!(read_tx.call(&mut store, ()).unwrap(), 66);
    }

    #[test]
    fn mock_transaction_distinguishes_imported_and_local_tmemory_overlays() {
        let engine = crate::Engine::default();
        let provider = transaction_test_module(
            &engine,
            r#"
            (module
              (tmemory $tx 1)
              (export "tx" (memory $tx)))
            "#,
        );
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (import "env" "tx" (tmemory $imported 1))
              (tmemory $local 1)
              (tfunc (export "write_both")
                (i32.tstore $imported (i32.const 0) (i32.const 11))
                (i32.tstore $local (i32.const 0) (i32.const 22)))
              (tfunc (export "read_imported") (result i32)
                (i32.tload $imported (i32.const 0)))
              (tfunc (export "read_local") (result i32)
                (i32.tload $local (i32.const 0))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let provider = crate::Instance::new(&mut store, &provider, &[]).unwrap();
        let tx = provider.get_memory(&mut store, "tx").unwrap();
        let instance = crate::Instance::new(&mut store, &module, &[tx.into()]).unwrap();
        let write_both = instance
            .get_typed_func::<(), ()>(&mut store, "write_both")
            .unwrap();
        let read_imported = instance
            .get_typed_func::<(), i32>(&mut store, "read_imported")
            .unwrap();
        let read_local = instance
            .get_typed_func::<(), i32>(&mut store, "read_local")
            .unwrap();

        write_both.call(&mut store, ()).unwrap();

        assert_eq!(read_imported.call(&mut store, ()).unwrap(), 11);
        assert_eq!(read_local.call(&mut store, ()).unwrap(), 22);
    }

    #[test]
    fn default_transaction_config_uses_minimal_vmemory_runtime() {
        let config = TransactionConfig::default();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::VMemory);
        assert_eq!(
            config.tmemory_persistence_mode(),
            TMemoryPersistenceMode::ResearchPretendPmem
        );
        assert_eq!(config.concurrency_control(), ConcurrencyControl::LockBased);
        assert_eq!(
            config.durability_policy(),
            DurabilityPolicy::VolatileRollbackOnly
        );
        assert_eq!(
            config.conflict_policy(),
            ConflictPolicy::AbortOrWizardDefault
        );
        assert_eq!(
            config.object_index_persistence_policy(),
            ObjectIndexPersistencePolicy::RebuildOnRecovery
        );
    }

    #[test]
    fn transaction_config_rejects_generic_file_backed_backend_selection() {
        assert!(
            TransactionConfig::with_tmemory_backend(TMemoryBackend::VMemory)
                .unwrap()
                .is_vmemory_only()
        );

        let error =
            TransactionConfig::with_tmemory_backend(TMemoryBackend::FileBackedMemory).unwrap_err();
        assert!(error.to_string().contains("requires explicit file backing"));
    }

    #[test]
    fn transaction_config_accepts_file_backed_temp_mode() {
        let config = TransactionConfig::with_file_backed_tmemory_temp().unwrap();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
        assert_eq!(
            config.tmemory_file_backing(),
            Some(TMemoryFileBacking::Temp)
        );
        assert!(!config.is_vmemory_only());
    }

    #[test]
    fn transaction_config_accepts_file_backed_path_mode() {
        let path = std::path::PathBuf::from("/tmp/wasmtime-transaction-file-backed-test.tmemory");
        let config = TransactionConfig::with_file_backed_tmemory_path(path.clone()).unwrap();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::FileBackedMemory);
        assert_eq!(
            config.tmemory_file_backing(),
            Some(TMemoryFileBacking::Path(path))
        );
        assert!(!config.is_vmemory_only());
    }

    #[test]
    fn generic_file_backed_backend_selection_still_requires_file_mode() {
        let error =
            TransactionConfig::with_tmemory_backend(TMemoryBackend::FileBackedMemory).unwrap_err();
        assert!(error.to_string().contains("requires explicit file backing"));
    }

    #[test]
    fn transaction_config_accepts_nvmemory_backend() {
        let config = TransactionConfig::with_tmemory_backend(TMemoryBackend::NVMemory).unwrap();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::NVMemory);
        assert_eq!(
            config.tmemory_persistence_mode(),
            TMemoryPersistenceMode::ResearchPretendPmem
        );
        assert!(!config.is_vmemory_only());
    }

    #[test]
    fn transaction_config_accepts_nvmemory_hardware_persistence_mode() {
        let config = TransactionConfig::with_nvmemory_persistence_mode(
            TMemoryPersistenceMode::RequireHardwarePmem,
        )
        .unwrap();

        assert_eq!(config.tmemory_backend(), TMemoryBackend::NVMemory);
        assert_eq!(
            config.tmemory_persistence_mode(),
            TMemoryPersistenceMode::RequireHardwarePmem
        );
        assert!(!config.is_vmemory_only());
    }

    #[test]
    fn transaction_config_defaults_to_rebuild_on_recovery_object_index_policy() {
        let config = TransactionConfig::default();
        assert_eq!(
            config.object_index_persistence_policy(),
            ObjectIndexPersistencePolicy::RebuildOnRecovery
        );
    }

    #[test]
    fn transaction_config_rejects_persistent_index_policy_for_now() {
        let error = TransactionConfig::with_object_index_persistence_policy(
            ObjectIndexPersistencePolicy::PersistentIndex,
        )
        .unwrap_err();
        assert!(error.to_string().contains("PersistentIndex"));
    }

    #[test]
    fn begin_commit_and_abort_clear_active_transaction() {
        clear_current_thread_transaction_for_test();
        let mut state = TransactionState::default();

        let first = state.begin().unwrap();
        assert_eq!(state.active_transaction(), Some(first));
        assert_eq!(current_thread_transaction_for_test(), Some(first));
        state.commit().unwrap();
        assert_eq!(state.active_transaction(), None);
        assert_eq!(current_thread_transaction_for_test(), None);

        let second = state.begin().unwrap();
        assert_eq!(state.active_transaction(), Some(second));
        assert_eq!(current_thread_transaction_for_test(), Some(second));
        state.abort().unwrap();
        assert_eq!(state.active_transaction(), None);
        assert_eq!(current_thread_transaction_for_test(), None);
    }

    #[test]
    fn commit_applies_only_latest_staged_records() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 1).unwrap();
        state.stage_memory_size(0, 2).unwrap();
        state.stage_global(3, GlobalSnapshot::I64(11)).unwrap();
        state.stage_global(3, GlobalSnapshot::I64(22)).unwrap();
        state
            .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
            .unwrap();
        state
            .stage_memory_granule(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
            .unwrap();

        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            applied,
            [
                StagedRecord::Global {
                    owner_instance: None,
                    global_index: 3,
                    value: GlobalSnapshot::I64(22)
                },
                StagedRecord::MemoryGranule {
                    owner_instance: None,
                    memory_index: 0,
                    granule_index: 7,
                    bytes: vec![0x22; TMEMORY_GRANULE_SIZE]
                },
                StagedRecord::MemorySize {
                    owner_instance: None,
                    memory_index: 0,
                    new_pages: 2
                },
            ]
        );
    }

    #[test]
    fn abort_drops_staged_records_without_apply() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 9).unwrap();
        state.stage_global(3, GlobalSnapshot::I32(4)).unwrap();
        state
            .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.abort().unwrap();
        assert_eq!(state.active_transaction(), None);

        state.begin().unwrap();
        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert!(applied.is_empty());
    }

    #[test]
    fn duplicate_granule_write_updates_one_staged_buffer() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(
            state
                .stage_memory_granule(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
                .unwrap()
        );
        assert!(
            !state
                .stage_memory_granule(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
                .unwrap()
        );

        let staged = state.staged_memory_granule(0, 7).unwrap();
        assert_eq!(staged, &[0x22; TMEMORY_GRANULE_SIZE]);
    }

    #[test]
    fn memory_granule_acquisition_requires_active_transaction() {
        let mut state = TransactionState::default();

        assert!(state.acquire_memory_granule_read(0, 7).is_err());
        assert!(
            state
                .acquire_memory_granule_write(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
                .is_err()
        );
    }

    #[test]
    fn read_and_write_granule_acquisition_tracks_owned_sets() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.acquire_memory_granule_read(0, 7).unwrap());
        assert!(!state.acquire_memory_granule_read(0, 7).unwrap());
        assert!(state.owns_memory_granule_read(0, 7));
        assert!(!state.owns_memory_granule_write(0, 7));

        assert!(
            state
                .acquire_memory_granule_write(0, 7, vec![0x11; TMEMORY_GRANULE_SIZE])
                .unwrap()
        );
        assert!(
            !state
                .acquire_memory_granule_write(0, 7, vec![0x22; TMEMORY_GRANULE_SIZE])
                .unwrap()
        );
        assert!(state.owns_memory_granule_read(0, 7));
        assert!(state.owns_memory_granule_write(0, 7));

        assert_eq!(
            state.staged_memory_granule(0, 7).unwrap(),
            &[0x22; TMEMORY_GRANULE_SIZE]
        );
    }

    #[test]
    fn stage_memory_write_splits_cross_granule_write() {
        let mut backing = vec![0x11; TMEMORY_GRANULE_SIZE * 2];
        backing[TMEMORY_GRANULE_SIZE..].fill(0x22);
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 2).unwrap();
        state
            .stage_memory_write(0, addr, &[0xaa, 0xbb, 0xcc, 0xdd], &backing)
            .unwrap();

        let first = state.staged_memory_granule(0, 0).unwrap();
        let second = state.staged_memory_granule(0, 1).unwrap();
        assert_eq!(
            &first[..TMEMORY_GRANULE_SIZE - 2],
            &backing[..TMEMORY_GRANULE_SIZE - 2]
        );
        assert_eq!(&first[TMEMORY_GRANULE_SIZE - 2..], &[0xaa, 0xbb]);
        assert_eq!(&second[..2], &[0xcc, 0xdd]);
        assert_eq!(&second[2..], &backing[TMEMORY_GRANULE_SIZE + 2..]);
        assert!(state.owns_memory_granule_read(0, 0));
        assert!(state.owns_memory_granule_write(0, 0));
        assert!(state.owns_memory_granule_read(0, 1));
        assert!(state.owns_memory_granule_write(0, 1));
    }

    #[test]
    fn read_memory_overlay_preserves_unwritten_staged_bytes() {
        let backing = vec![0x10; TMEMORY_GRANULE_SIZE];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        state
            .stage_memory_write(0, 10, &[0xaa, 0xbb], &backing)
            .unwrap();
        state.stage_memory_write(0, 12, &[0xcc], &backing).unwrap();

        let read = state.read_memory_overlay(0, 8, 6, &backing).unwrap();
        let mut expected_read = backing[8..14].to_vec();
        expected_read[2..5].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(read, expected_read);

        let mut expected_granule = backing.clone();
        expected_granule[10..13].copy_from_slice(&[0xaa, 0xbb, 0xcc]);
        assert_eq!(
            state.staged_memory_granule(0, 0).unwrap(),
            expected_granule.as_slice()
        );
    }

    #[test]
    fn memory_overlay_helpers_reject_out_of_bounds_access() {
        let backing = vec![0; 8];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let write_error = state
            .stage_memory_write(0, 7, &[0xaa, 0xbb], &backing)
            .unwrap_err();
        assert!(
            write_error
                .to_string()
                .contains("out of bounds tmemory access")
        );

        let read_error = state.read_memory_overlay(0, 7, 2, &backing).unwrap_err();
        assert!(
            read_error
                .to_string()
                .contains("out of bounds tmemory access")
        );
    }

    #[test]
    fn read_memory_overlay_tracks_read_ownership_for_touched_granules() {
        let backing = vec![0; TMEMORY_GRANULE_SIZE * 2];
        let mut state = TransactionState::default();
        state.begin().unwrap();

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
        assert_eq!(
            state.read_memory_overlay(0, addr, 2, &backing).unwrap(),
            vec![0, 0]
        );

        assert!(state.owns_memory_granule_read(0, 0));
        assert!(state.owns_memory_granule_read(0, 1));
        assert!(!state.owns_memory_granule_write(0, 0));
        assert!(!state.owns_memory_granule_write(0, 1));
    }

    #[test]
    fn pending_memory_store_scratch_acquires_write_ownership_for_touched_granules() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        let owner = InstanceId::from_u32(1);

        let addr = u64::try_from(TMEMORY_GRANULE_SIZE - 1).unwrap();
        state
            .set_memory_store_scratch(owner, 0, addr, vec![0xaa, 0xbb])
            .unwrap();

        assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 0));
        assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 1));
    }

    #[test]
    fn commit_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.commit().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
    }

    #[test]
    fn commit_rejects_changed_optimistic_read_version() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();

        let error = state.commit().unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn commit_validates_optimistic_read_versions_before_clearing() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();

        let error = state
            .commit_with_read_validation(|_| Ok(2), |_| Ok(()))
            .unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn commit_validates_reads_before_apply_callback() {
        let mut state = TransactionState::default();
        let transaction = state.begin().unwrap();
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        state.stage_global(3, GlobalSnapshot::I32(7)).unwrap();
        state
            .locks
            .record_read_for_test(transaction, granule, 1)
            .unwrap();
        let mut applied = false;

        let error = state
            .commit_with_read_validation(
                |_| Ok(2),
                |_| {
                    applied = true;
                    Ok(())
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("transaction read conflict"));
        assert!(!applied);
        assert_eq!(state.active_transaction(), Some(transaction));
    }

    #[test]
    fn abort_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.abort().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
    }

    #[test]
    fn fail_releases_lock_based_ownership_for_next_transaction() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();

        state.fail().unwrap();

        state.begin().unwrap();
        state.acquire_memory_granule_read(0, 0).unwrap();
    }

    #[test]
    fn lock_based_transaction_ids_keep_separate_workspaces() {
        clear_current_thread_transaction_for_test();
        let mut state = TransactionState::default();
        let first = TransactionId::from_raw(11);
        let second = TransactionId::from_raw(22);

        assert_eq!(state.enter_transaction(first).unwrap(), None);
        assert_eq!(current_thread_transaction_for_test(), Some(first));
        state.stage_global(0, GlobalSnapshot::I32(11)).unwrap();

        assert_eq!(state.enter_transaction(second).unwrap(), Some(first));
        assert_eq!(current_thread_transaction_for_test(), Some(second));
        state.stage_global(1, GlobalSnapshot::I32(22)).unwrap();
        assert_eq!(state.staged_global_owned(None, 0), None);
        assert_eq!(
            state.staged_global_owned(None, 1),
            Some(GlobalSnapshot::I32(22))
        );

        state.restore_transaction(Some(first)).unwrap();
        assert_eq!(current_thread_transaction_for_test(), Some(first));
        assert_eq!(
            state.staged_global_owned(None, 0),
            Some(GlobalSnapshot::I32(11))
        );
        assert_eq!(state.staged_global_owned(None, 1), None);

        state.restore_transaction(None).unwrap();
        assert_eq!(state.active_transaction(), None);
        assert_eq!(current_thread_transaction_for_test(), None);
        assert!(state.transaction_is_open(first));
        assert!(state.transaction_is_open(second));
    }

    #[test]
    fn lower_transaction_id_aborts_higher_suspended_writer() {
        clear_current_thread_transaction_for_test();
        let mut state = TransactionState::default();
        let higher = TransactionId::from_raw(100_001);
        let lower = TransactionId::from_raw(1);

        state.enter_transaction(higher).unwrap();
        state.stage_global(0, GlobalSnapshot::I32(100)).unwrap();
        state.restore_transaction(None).unwrap();
        assert!(state.transaction_is_open(higher));

        state.enter_transaction(lower).unwrap();
        state.stage_global(0, GlobalSnapshot::I32(200)).unwrap();

        assert!(!state.transaction_is_open(higher));
        assert!(state.transaction_is_open(lower));
    }

    #[test]
    fn lower_transaction_id_reader_records_version_after_aborting_writer() {
        clear_current_thread_transaction_for_test();
        let mut state = TransactionState::default();
        let higher = TransactionId::from_raw(100_001);
        let lower = TransactionId::from_raw(1);

        state.enter_transaction(higher).unwrap();
        state.stage_global(0, GlobalSnapshot::I32(100)).unwrap();
        state.restore_transaction(None).unwrap();

        state.enter_transaction(lower).unwrap();
        state.acquire_global_read_owned(None, 0).unwrap();
        assert!(!state.transaction_is_open(higher));
        state
            .validate_active_read(
                global_granule_id(None, 0),
                state.versioned_granule_version(global_granule_id(None, 0)),
            )
            .unwrap();
    }

    #[test]
    fn aborting_suspended_transaction_releases_only_its_locks() {
        let mut state = TransactionState::default();
        let first = TransactionId::from_raw(11);
        let second = TransactionId::from_raw(22);

        state.enter_transaction(first).unwrap();
        state
            .acquire_memory_granule_write(0, 0, vec![0xaa; TMEMORY_GRANULE_SIZE])
            .unwrap();
        state.enter_transaction(second).unwrap();
        state
            .acquire_memory_granule_write(0, 1, vec![0xbb; TMEMORY_GRANULE_SIZE])
            .unwrap();

        let conflict = state.acquire_memory_granule_read(0, 0).unwrap_err();
        assert!(conflict.to_string().contains("transaction read conflict"));

        assert!(state.abort_transaction(first).unwrap());
        state.acquire_memory_granule_read(0, 0).unwrap();
        assert!(state.owns_memory_granule_write(0, 1));
        assert!(!state.abort_transaction(first).unwrap());
    }

    #[test]
    fn fail_drops_staged_records() {
        let mut state = TransactionState::default();
        state.begin().unwrap();
        state.stage_memory_size(0, 9).unwrap();

        state.fail().unwrap();

        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn duplicate_global_write_updates_latest_staged_value() {
        let mut state = TransactionState::default();
        state.begin().unwrap();

        assert!(state.stage_global(3, GlobalSnapshot::I64(11)).unwrap());
        assert!(!state.stage_global(3, GlobalSnapshot::I64(22)).unwrap());

        let mut applied = Vec::new();
        state
            .commit_with(|record| {
                applied.push(record.clone());
                Ok(())
            })
            .unwrap();

        assert_eq!(
            applied,
            [StagedRecord::Global {
                owner_instance: None,
                global_index: 3,
                value: GlobalSnapshot::I64(22)
            }]
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_globals() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        state.begin().unwrap();

        assert!(
            state
                .stage_global_owned(None, 0, GlobalSnapshot::I32(1))
                .unwrap()
        );
        assert!(
            state
                .stage_global_owned(Some(owner0), 0, GlobalSnapshot::I32(2))
                .unwrap()
        );

        assert_eq!(
            state.staged_global_owned(None, 0),
            Some(GlobalSnapshot::I32(1))
        );
        assert_eq!(
            state.staged_global_owned(Some(owner0), 0),
            Some(GlobalSnapshot::I32(2))
        );

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::Global {
            owner_instance: None,
            global_index: 0,
            value: GlobalSnapshot::I32(1),
        }));
        assert!(records.contains(&StagedRecord::Global {
            owner_instance: Some(owner0),
            global_index: 0,
            value: GlobalSnapshot::I32(2),
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::Global {
                        global_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_memory_granules() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        let ownerless_bytes = vec![0x11; TMEMORY_GRANULE_SIZE];
        let owner0_bytes = vec![0x22; TMEMORY_GRANULE_SIZE];
        state.begin().unwrap();

        assert!(
            state
                .stage_memory_granule(0, 0, ownerless_bytes.clone())
                .unwrap()
        );
        assert!(
            state
                .acquire_memory_granule_write_owned(Some(owner0), 0, 0, owner0_bytes.clone())
                .unwrap()
        );

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::MemoryGranule {
            owner_instance: None,
            memory_index: 0,
            granule_index: 0,
            bytes: ownerless_bytes,
        }));
        assert!(records.contains(&StagedRecord::MemoryGranule {
            owner_instance: Some(owner0),
            memory_index: 0,
            granule_index: 0,
            bytes: owner0_bytes,
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::MemoryGranule {
                        memory_index: 0,
                        granule_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn owner_instance_zero_does_not_alias_ownerless_memory_sizes() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        state.begin().unwrap();

        assert!(state.stage_memory_size_owned(None, 0, 1).unwrap());
        assert!(state.stage_memory_size_owned(Some(owner0), 0, 2).unwrap());

        let records = state.staged_records().unwrap();
        assert!(records.contains(&StagedRecord::MemorySize {
            owner_instance: None,
            memory_index: 0,
            new_pages: 1,
        }));
        assert!(records.contains(&StagedRecord::MemorySize {
            owner_instance: Some(owner0),
            memory_index: 0,
            new_pages: 2,
        }));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record,
                    StagedRecord::MemorySize {
                        memory_index: 0,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn workspace_merges_staged_and_committed_granules_with_nonzero_backing_base() {
        let mut state = TransactionState::default();
        let owner0 = InstanceId::from_u32(0);
        let memory_index = 0;
        let backing_base = TMEMORY_GRANULE_SIZE as u64;
        let memory_len = TMEMORY_GRANULE_SIZE * 4;
        let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 2)
            .map(|i| (i % 251) as u8)
            .collect();
        let addr = (TMEMORY_GRANULE_SIZE * 2 - 2) as u64;
        let staged = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let read_addr = addr - 2;
        let read_len = 10;
        state.begin().unwrap();

        state
            .stage_memory_write_owned_from_backing(
                Some(owner0),
                memory_index,
                addr,
                &staged,
                backing_base,
                &committed,
                memory_len,
            )
            .unwrap();

        let merged = state
            .read_memory_overlay_owned_from_backing(
                Some(owner0),
                memory_index,
                read_addr,
                read_len,
                backing_base,
                &committed,
                memory_len,
            )
            .unwrap();

        let read_start = read_addr as usize - backing_base as usize;
        let mut expected = committed[read_start..read_start + read_len].to_vec();
        let staged_offset = (addr - read_addr) as usize;
        expected[staged_offset..staged_offset + staged.len()].copy_from_slice(&staged);

        assert_eq!(merged, expected);
    }

    #[test]
    fn granule_id_orders_by_object_space_and_index() {
        let mut ids = vec![
            GranuleId::TGlobal {
                instance: None,
                global_index: 2,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 4,
            },
            GranuleId::TMemory {
                instance: Some(2),
                memory_index: 0,
                granule_index: 0,
            },
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 3,
            },
            GranuleId::TGlobal {
                instance: None,
                global_index: 1,
            },
            GranuleId::TMemorySize {
                instance: None,
                memory_index: 1,
            },
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 7,
                granule_index: 1,
            },
            GranuleId::TArray {
                object_id: ObjectId { object_index: 2 },
            },
            GranuleId::TStruct {
                object_id: ObjectId { object_index: 1 },
            },
            GranuleId::TTableSize {
                instance: None,
                table_index: 3,
            },
            GranuleId::TTable {
                instance: Some(1),
                table_index: 2,
                granule_index: 0,
            },
        ];

        ids.sort();

        assert_eq!(
            ids,
            vec![
                GranuleId::TMemory {
                    instance: Some(1),
                    memory_index: 7,
                    granule_index: 1,
                },
                GranuleId::TMemory {
                    instance: Some(1),
                    memory_index: 7,
                    granule_index: 3,
                },
                GranuleId::TMemory {
                    instance: Some(2),
                    memory_index: 0,
                    granule_index: 0,
                },
                GranuleId::TMemorySize {
                    instance: None,
                    memory_index: 1,
                },
                GranuleId::TMemorySize {
                    instance: None,
                    memory_index: 4,
                },
                GranuleId::TGlobal {
                    instance: None,
                    global_index: 1,
                },
                GranuleId::TGlobal {
                    instance: None,
                    global_index: 2,
                },
                GranuleId::TTable {
                    instance: Some(1),
                    table_index: 2,
                    granule_index: 0,
                },
                GranuleId::TTableSize {
                    instance: None,
                    table_index: 3,
                },
                GranuleId::TStruct {
                    object_id: ObjectId { object_index: 1 },
                },
                GranuleId::TArray {
                    object_id: ObjectId { object_index: 2 },
                },
            ]
        );
    }

    #[test]
    fn workspace_merges_staged_and_committed_granules() {
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));
        let instance = 11;
        let memory_index = 3;
        let committed: Vec<u8> = (0..TMEMORY_GRANULE_SIZE * 3)
            .map(|i| (i % 251) as u8)
            .collect();

        state.stage_granule_for_test(
            GranuleId::TMemory {
                instance: Some(instance),
                memory_index,
                granule_index: 0,
            },
            vec![0xAA; TMEMORY_GRANULE_SIZE],
        );
        state.stage_granule_for_test(
            GranuleId::TMemory {
                instance: Some(instance),
                memory_index,
                granule_index: 2,
            },
            vec![0xCC; TMEMORY_GRANULE_SIZE],
        );

        let merged = state
            .read_tmemory_range_for_test(
                Some(instance),
                memory_index,
                (TMEMORY_GRANULE_SIZE - 2) as u64,
                TMEMORY_GRANULE_SIZE + 6,
                &committed,
            )
            .unwrap();

        let mut expected = vec![0xAA; 2];
        expected.extend_from_slice(&committed[TMEMORY_GRANULE_SIZE..TMEMORY_GRANULE_SIZE * 2]);
        expected.extend_from_slice(&[0xCC; 4]);

        assert_eq!(merged, expected);
    }

    #[test]
    fn transaction_commit_copies_staged_granules_to_tmemory() {
        let mut tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

        state
            .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
            .expect("stage tmemory");
        state
            .commit_tmemory_for_test(&mut tmemory)
            .expect("commit tmemory");

        assert_eq!(
            tmemory.read_committed(4..8).expect("read committed"),
            vec![9, 8, 7, 6]
        );
    }

    #[test]
    fn transaction_commit_uses_persistent_tmemory_undo_for_nvmemory() {
        let mut tmemory = crate::runtime::vm::TMemory::new_with_backend_limits(
            TMemoryBackend::NVMemory,
            1,
            Some(1),
        )
        .expect("tmemory");
        tmemory.commit_range(0, &[1, 2, 3, 4]).unwrap();
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(12));

        state
            .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
            .expect("stage tmemory");
        state
            .commit_tmemory_for_test(&mut tmemory)
            .expect("commit tmemory");

        assert_eq!(
            tmemory.read_committed(4..8).expect("read committed"),
            vec![9, 8, 7, 6]
        );
        assert_eq!(state.durable_log_entries_for_test(12).len(), 2);
    }

    #[test]
    fn transaction_abort_discards_staged_tmemory_bytes() {
        let tmemory = crate::runtime::vm::TMemory::new_vmemory(1).expect("tmemory");
        let mut state = TransactionState::new_for_test(TransactionId::from_raw(7));

        state
            .stage_tmemory_write_for_test(0, 0, 4, &[9, 8, 7, 6], &tmemory)
            .expect("stage tmemory");
        state.abort().expect("abort transaction");

        assert_eq!(
            tmemory.read_committed(4..8).expect("read committed"),
            vec![0, 0, 0, 0]
        );
    }

    #[test]
    fn transaction_table_granules_follow_wizard_sixteen_element_chunks() {
        let owner = InstanceId::from_u32(2);

        assert_eq!(
            table_granule_id(Some(owner), 3, 0),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 0,
            }
        );
        assert_eq!(
            table_granule_id(Some(owner), 3, 15),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 0,
            }
        );
        assert_eq!(
            table_granule_id(Some(owner), 3, 16),
            GranuleId::TTable {
                instance: Some(2),
                table_index: 3,
                granule_index: 1,
            }
        );
    }

    #[test]
    fn transaction_table_granule_acquisition_tracks_read_and_write_sets() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(1);
        state.begin().unwrap();

        assert!(
            state
                .acquire_table_granule_read_owned(Some(owner), 2, 15, 4)
                .unwrap()
        );
        assert!(
            !state
                .acquire_table_granule_read_owned(Some(owner), 2, 8, 4)
                .unwrap()
        );
        assert!(state.owns_table_granule_read_owned(Some(owner), 2, 0));
        assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));

        assert!(
            state
                .acquire_table_granule_write_owned(Some(owner), 2, 16, 7)
                .unwrap()
        );
        assert!(state.owns_table_granule_read_owned(Some(owner), 2, 1));
        assert!(state.owns_table_granule_write_owned(Some(owner), 2, 1));
    }

    #[test]
    fn transaction_table_size_granule_is_separate_from_element_granules() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(1);
        state.begin().unwrap();

        assert!(
            state
                .acquire_table_size_read_owned(Some(owner), 2, 11)
                .unwrap()
        );
        assert!(state.owns_table_size_read_owned(Some(owner), 2));
        assert!(!state.owns_table_granule_read_owned(Some(owner), 2, 0));

        assert!(
            state
                .acquire_table_size_write_owned(Some(owner), 2, 11)
                .unwrap()
        );
        assert!(state.owns_table_size_write_owned(Some(owner), 2));
        assert!(!state.owns_table_granule_write_owned(Some(owner), 2, 0));
    }

    #[test]
    fn granule_permission_acquisition_tracks_all_granule_kinds() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(1);
        let object = ObjectId { object_index: 9 };
        let granules = [
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 0,
                granule_index: 2,
            },
            GranuleId::TMemorySize {
                instance: Some(1),
                memory_index: 0,
            },
            GranuleId::TGlobal {
                instance: Some(1),
                global_index: 3,
            },
            GranuleId::TTable {
                instance: Some(1),
                table_index: 4,
                granule_index: 5,
            },
            GranuleId::TTableSize {
                instance: Some(1),
                table_index: 4,
            },
            GranuleId::TStruct { object_id: object },
            GranuleId::TArray { object_id: object },
        ];

        state.begin().unwrap();

        for granule in granules {
            assert!(state.acquire_granule_read(granule, 7).unwrap());
            assert!(!state.acquire_granule_read(granule, 7).unwrap());
            assert!(state.owns_granule_read(granule));
            assert!(!state.owns_granule_write(granule));

            assert!(state.acquire_granule_write(granule, 7).unwrap());
            assert!(!state.acquire_granule_write(granule, 7).unwrap());
            assert!(state.owns_granule_read(granule));
            assert!(state.owns_granule_write(granule));
        }

        assert!(state.owns_memory_granule_write_owned(Some(owner), 0, 2));
        assert!(state.owns_table_granule_write_owned(Some(owner), 4, 5));
        assert!(state.owns_table_size_write_owned(Some(owner), 4));
    }

    #[test]
    fn stage_global_acquires_tglobal_write_permission() {
        let mut state = TransactionState::default();
        let owner = InstanceId::from_u32(3);
        let granule = GranuleId::TGlobal {
            instance: Some(3),
            global_index: 1,
        };

        state.begin().unwrap();

        state
            .stage_global_owned(Some(owner), 1, GlobalSnapshot::I32(42))
            .unwrap();

        assert!(state.owns_granule_read(granule));
        assert!(state.owns_granule_write(granule));
    }

    #[test]
    fn tref_cast_acquires_object_permission_for_known_refs() {
        let mut objects = ObjectTable::default();
        let struct_object = objects
            .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
            .unwrap();
        let array_object = objects
            .allocate_persistent_array_for_gc_ref(0x22, vec![ObjectValue::I32(9)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        assert!(state.acquire_tref_read_for_gc_ref(&objects, 0x11).unwrap());
        assert!(state.owns_struct_read(struct_object));
        assert!(!state.owns_struct_write(struct_object));

        assert!(state.acquire_tref_write_for_gc_ref(&objects, 0x22).unwrap());
        assert!(state.owns_array_read(array_object));
        assert!(state.owns_array_write(array_object));
    }

    #[test]
    fn tref_cast_ignores_null_and_unknown_refs_without_granting_access() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        assert!(!state.acquire_tref_read_for_gc_ref(&objects, 0).unwrap());
        assert!(!state.acquire_tref_write_for_gc_ref(&objects, 0x99).unwrap());
        assert!(!state.owns_struct_read(object));
        assert!(!state.owns_struct_write(object));
    }

    #[test]
    fn tref_cast_does_not_lock_volatile_gc_backed_objects() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        assert!(!objects.is_persistent(object).unwrap());
        assert!(!state.acquire_tref_read_for_gc_ref(&objects, 0x11).unwrap());
        assert!(!state.acquire_tref_write_for_gc_ref(&objects, 0x11).unwrap());
        assert!(!state.owns_struct_read(object));
        assert!(!state.owns_struct_write(object));
        assert_eq!(
            state.read_struct_field(&objects, object, 0).unwrap(),
            ObjectValue::I32(7)
        );
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
            .unwrap();
    }

    #[test]
    fn object_payload_access_requires_prior_tref_permission() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x11, vec![ObjectValue::I32(7)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        let error = state.read_struct_field(&objects, object, 0).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transactional object read permission was not acquired")
        );

        state.acquire_tref_read_for_gc_ref(&objects, 0x11).unwrap();
        assert_eq!(
            state.read_struct_field(&objects, object, 0).unwrap(),
            ObjectValue::I32(7)
        );

        let error = state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("transactional object write permission was not acquired")
        );

        state.acquire_tref_write_for_gc_ref(&objects, 0x11).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(8))
            .unwrap();
        assert_eq!(
            state.read_struct_field(&objects, object, 0).unwrap(),
            ObjectValue::I32(8)
        );
    }

    #[test]
    fn object_table_allocates_dense_stable_ids_and_reuses_freed_slots() {
        let mut objects = ObjectTable::default();

        let first = objects.allocate(ObjectKind::Struct).unwrap();
        let second = objects.allocate(ObjectKind::Array).unwrap();

        assert_eq!(first, ObjectId { object_index: 0 });
        assert_eq!(second, ObjectId { object_index: 1 });
        assert_eq!(objects.live_count(), 2);
        assert_eq!(objects.slot_count(), 2);
        assert_eq!(objects.kind(first).unwrap(), ObjectKind::Struct);
        assert_eq!(objects.kind(second).unwrap(), ObjectKind::Array);

        let second_version = objects.version(second).unwrap();
        assert!(objects.free(second).unwrap());
        assert_eq!(objects.live_count(), 1);
        assert_eq!(objects.slot_count(), 2);
        assert!(objects.kind(second).is_err());

        let reused = objects.allocate(ObjectKind::Struct).unwrap();

        assert_eq!(reused, second);
        assert_eq!(objects.live_count(), 2);
        assert_eq!(objects.slot_count(), 2);
        assert!(objects.version(reused).unwrap() > second_version);
        assert_eq!(objects.kind(reused).unwrap(), ObjectKind::Struct);
    }

    #[test]
    fn object_table_granule_permissions_are_kind_aware() {
        let mut objects = ObjectTable::default();
        let struct_object = objects.allocate(ObjectKind::Struct).unwrap();
        let array_object = objects.allocate(ObjectKind::Array).unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        assert!(state.acquire_object_read(&objects, struct_object).unwrap());
        assert!(state.owns_granule_read(GranuleId::TStruct {
            object_id: struct_object,
        }));
        assert!(!state.owns_granule_write(GranuleId::TStruct {
            object_id: struct_object,
        }));

        assert!(state.acquire_object_write(&objects, array_object).unwrap());
        assert!(state.owns_granule_read(GranuleId::TArray {
            object_id: array_object,
        }));
        assert!(state.owns_granule_write(GranuleId::TArray {
            object_id: array_object,
        }));

        state.abort().unwrap();

        assert!(!state.owns_granule_read(GranuleId::TStruct {
            object_id: struct_object,
        }));
        assert!(!state.owns_granule_write(GranuleId::TArray {
            object_id: array_object,
        }));
    }

    #[test]
    fn transaction_object_abi_encodes_null_and_object_ids() {
        let null = ObjectRefValue::from_optional_object_id(None).unwrap();
        assert_eq!(null.as_raw(), 0);
        assert_eq!(ObjectRefValue::from_raw(0).decode(), None);

        let object = ObjectId { object_index: 41 };
        let encoded = ObjectRefValue::from_optional_object_id(Some(object)).unwrap();
        assert_eq!(encoded.as_raw(), 42);
        assert_eq!(encoded.decode(), Some(object));

        let max_encodable_object = ObjectId {
            object_index: u64::MAX - 1,
        };
        let encoded = ObjectRefValue::from_optional_object_id(Some(max_encodable_object)).unwrap();
        assert_eq!(encoded.as_raw(), u64::MAX);
        assert_eq!(encoded.decode(), Some(max_encodable_object));
    }

    #[test]
    fn transaction_object_abi_rejects_ref_encoding_overflow() {
        let error = ObjectRefValue::from_optional_object_id(Some(ObjectId {
            object_index: u64::MAX,
        }))
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("object reference encoding overflow")
        );
    }

    #[test]
    fn transaction_object_abi_layout_is_explicit() {
        assert_eq!(
            core::mem::size_of::<ObjectRefValue>(),
            core::mem::size_of::<u64>()
        );
        assert_eq!(
            core::mem::align_of::<ObjectRefValue>(),
            core::mem::align_of::<u64>()
        );
        assert_eq!(core::mem::size_of::<ObjectValueAbi>(), 24);
        assert_eq!(
            core::mem::align_of::<ObjectValueAbi>(),
            core::mem::align_of::<u64>()
        );
    }

    #[test]
    fn transaction_object_abi_roundtrips_object_values() {
        let values = [
            ObjectValue::I32(-17),
            ObjectValue::I64(-18),
            ObjectValue::F32(0x7fc0_0001),
            ObjectValue::F64(0x7ff8_0000_0000_0001),
            ObjectValue::V128([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            ObjectValue::Ref(None),
            ObjectValue::Ref(Some(ObjectId { object_index: 8 })),
        ];

        for value in values {
            let abi = ObjectValueAbi::from_object_value(&value).unwrap();
            assert_eq!(abi.to_object_value().unwrap(), value);
        }

        assert_eq!(
            ObjectValueAbi::from_object_value(&ObjectValue::Ref(Some(ObjectId {
                object_index: 8
            })))
            .unwrap()
            .as_parts(),
            (OBJECT_VALUE_ABI_TAG_REF, 9, 0)
        );
        assert_eq!(
            ObjectValueAbi::from_object_value(&ObjectValue::V128([
                0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            ]))
            .unwrap()
            .as_parts(),
            (
                OBJECT_VALUE_ABI_TAG_V128,
                0x0706_0504_0302_0100,
                0x0f0e_0d0c_0b0a_0908,
            )
        );
    }

    #[test]
    fn transaction_object_abi_rejects_unknown_and_noncanonical_values() {
        let error = ObjectValueAbi::from_parts(99, 0, 0).unwrap_err();
        assert!(error.to_string().contains("unknown object value ABI tag"));

        let error =
            ObjectValueAbi::from_parts(OBJECT_VALUE_ABI_TAG_I32, u64::from(u32::MAX) + 1, 0)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("non-canonical i32 object value ABI payload")
        );

        let error = ObjectValueAbi::from_parts(OBJECT_VALUE_ABI_TAG_REF, 0, 1).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("non-canonical ref object value ABI payload")
        );
    }

    #[test]
    fn object_payload_updates_are_copy_on_write_until_commit() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();
        assert_eq!(
            state.read_object_payload(&objects, object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        );
        state
            .stage_object_payload(
                &objects,
                object,
                ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)]),
            )
            .unwrap();
        assert_eq!(
            state.read_object_payload(&objects, object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(None)])
        );
        state.abort().unwrap();
        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        );

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_object_payload(
                &objects,
                object,
                ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)]),
            )
            .unwrap();
        assert!(state.commit_object_payloads(&mut objects).unwrap());
        state.complete_commit().unwrap();

        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(3), ObjectValue::Ref(None)])
        );
    }

    #[test]
    fn transaction_state_publishes_committed_object_payload_to_file_backed_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx-log.bin");
        let durable_log = TxDurableLog::create_file_backed(&path, 32).unwrap();
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(
                0x100,
                vec![ObjectValue::I32(1), ObjectValue::I64(2)],
            )
            .unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(12),
            durable_log,
        );

        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );
        let marker = state
            .publish_object_publications_before_commit(12, 12, &publications)
            .unwrap()
            .unwrap();
        state.publish_commit_lp(12, 12, marker).unwrap();
        drop(state);

        let recovered = TxDurableLog::recover_file_backed_for_test(&path).unwrap();
        assert_eq!(recovered.object_winners.len(), 1);
        assert_eq!(recovered.object_winners[0].object_id, object.object_index);
        assert_eq!(recovered.object_winners[0].version, 2);
    }

    #[test]
    fn transaction_state_does_not_publish_volatile_object_payload_to_durable_log() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );

        assert!(publications.is_empty());
    }

    #[test]
    fn file_backed_mixed_commit_recovers_tmemory_and_persistent_object() {
        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("tmemory.bin");
        let tx_log_path = dir.path().join("tx-log.bin");
        let mut tmemory = crate::runtime::vm::TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        let old_granule = vec![0x11; TMEMORY_GRANULE_SIZE];
        let new_granule = vec![0x22; TMEMORY_GRANULE_SIZE];
        tmemory
            .commit_staged_tmemory_granule(0, &old_granule)
            .unwrap();

        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(31),
            durable_log,
        );
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x101, vec![ObjectValue::I32(1)])
            .unwrap();

        let undo = tmemory
            .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
            .unwrap();
        let tmemory_marker = state
            .publish_tmemory_undo_before_in_place_write(31, 31, &undo)
            .unwrap();
        tmemory
            .commit_staged_tmemory_granule(0, &new_granule)
            .unwrap();

        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );
        let object_marker = state
            .publish_object_publications_before_commit(31, 31, &publications)
            .unwrap();
        state
            .publish_commit_lp(31, 31, object_marker.unwrap_or(tmemory_marker))
            .unwrap();
        drop(state);
        clear_current_thread_transaction_for_test();

        let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
        assert!(recovered.tmemory_undo_rollbacks.is_empty());
        tmemory
            .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                recovered.tmemory_undo_rollbacks,
            )
            .unwrap();
        assert_eq!(
            tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
            new_granule
        );
        let tmemory_file = std::fs::read(&tmemory_path).unwrap();
        assert_eq!(
            &tmemory_file[..TMEMORY_GRANULE_SIZE],
            new_granule.as_slice()
        );

        let object_winners =
            crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
                &tx_log_path,
            )
            .unwrap();
        let mut recovered_objects = ObjectTable::default();
        recovered_objects
            .rebuild_from_recovery_for_test(&object_winners)
            .unwrap();
        assert_eq!(
            recovered_objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(9)])
        );
    }

    #[test]
    fn file_backed_mixed_loose_end_rolls_back_tmemory_and_drops_object_publication() {
        let dir = tempfile::tempdir().unwrap();
        let tmemory_path = dir.path().join("tmemory.bin");
        let tx_log_path = dir.path().join("tx-log.bin");
        let mut tmemory = crate::runtime::vm::TMemory::new(
            TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
            1,
            Some(1),
        )
        .unwrap();
        let old_granule = vec![0x33; TMEMORY_GRANULE_SIZE];
        let new_granule = vec![0x44; TMEMORY_GRANULE_SIZE];
        tmemory
            .commit_staged_tmemory_granule(0, &old_granule)
            .unwrap();

        let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64).unwrap();
        let mut state = TransactionState::new_for_test_with_durable_log(
            TransactionId::from_raw(32),
            durable_log,
        );
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_persistent_struct_for_gc_ref(0x102, vec![ObjectValue::I32(1)])
            .unwrap();

        let undo = tmemory
            .prepare_tmemory_undo_record(Some(7), 0, 0, &new_granule)
            .unwrap();
        state
            .publish_tmemory_undo_before_in_place_write(32, 32, &undo)
            .unwrap();
        tmemory
            .commit_staged_tmemory_granule(0, &new_granule)
            .unwrap();

        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();
        let mut publications = Vec::new();
        assert!(
            state
                .commit_object_payloads_into(&mut objects, &mut publications)
                .unwrap()
        );
        state
            .publish_object_publications_before_commit(32, 32, &publications)
            .unwrap();
        drop(state);
        clear_current_thread_transaction_for_test();

        let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path).unwrap();
        assert_eq!(recovered.tmemory_undo_rollbacks.len(), 1);
        assert!(recovered.object_winners.is_empty());
        tmemory
            .apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                recovered.tmemory_undo_rollbacks,
            )
            .unwrap();
        assert_eq!(
            tmemory.read_committed(0..TMEMORY_GRANULE_SIZE).unwrap(),
            old_granule
        );
        let tmemory_file = std::fs::read(&tmemory_path).unwrap();
        assert_eq!(
            &tmemory_file[..TMEMORY_GRANULE_SIZE],
            old_granule.as_slice()
        );

        let object_winners =
            crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
                &tx_log_path,
            )
            .unwrap();
        assert!(object_winners.is_empty());
    }

    mod model_mixed_participants {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        struct ModelWorld {
            tmemory_granules: BTreeMap<u32, u8>,
            object_fields: BTreeMap<u64, i32>,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct ModelTMemoryWrite {
            granule: u32,
            old_byte: u8,
            new_byte: u8,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct ModelObjectWrite {
            object_id: u64,
            old_value: i32,
            new_value: i32,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct MixedModelCase {
            tmemory_writes: Vec<ModelTMemoryWrite>,
            object_writes: Vec<ModelObjectWrite>,
            committed: bool,
        }

        #[derive(Clone, Debug, Eq, PartialEq)]
        struct ActualMixedWorld {
            world: ModelWorld,
            object_winner_count: usize,
        }

        impl MixedModelCase {
            fn expected_world(&self) -> ModelWorld {
                let tmemory_granules = self
                    .tmemory_writes
                    .iter()
                    .map(|write| {
                        (
                            write.granule,
                            if self.committed {
                                write.new_byte
                            } else {
                                write.old_byte
                            },
                        )
                    })
                    .collect();
                let object_fields = if self.committed {
                    self.object_writes
                        .iter()
                        .map(|write| (write.object_id, write.new_value))
                        .collect()
                } else {
                    BTreeMap::new()
                };
                ModelWorld {
                    tmemory_granules,
                    object_fields,
                }
            }
        }

        fn repeated_granule(byte: u8) -> Vec<u8> {
            vec![byte; TMEMORY_GRANULE_SIZE]
        }

        fn file_backed_granule_byte(bytes: &[u8], granule: u32) -> Result<u8> {
            let granule = usize::try_from(granule)
                .context("tmemory model granule index does not fit host usize")?;
            let start = granule
                .checked_mul(TMEMORY_GRANULE_SIZE)
                .context("tmemory model granule start overflow")?;
            let end = start
                .checked_add(TMEMORY_GRANULE_SIZE)
                .context("tmemory model granule end overflow")?;
            ensure!(
                end <= bytes.len(),
                "tmemory model granule range is out of bounds"
            );
            let first = *bytes[start..end]
                .first()
                .context("tmemory model granule is unexpectedly empty")?;
            ensure!(
                bytes[start..end].iter().all(|byte| *byte == first),
                "tmemory model expected one repeated byte per granule"
            );
            Ok(first)
        }

        fn model_case_strategy() -> impl Strategy<Value = MixedModelCase> {
            (
                prop::collection::vec(any::<bool>(), 3)
                    .prop_filter("must write at least one tmemory granule", |flags| {
                        flags.iter().any(|flag| *flag)
                    }),
                prop::collection::vec(
                    (any::<u8>(), any::<u8>())
                        .prop_filter("old and new tmemory bytes must differ", |(old, new)| {
                            old != new
                        }),
                    3,
                ),
                prop::collection::vec(any::<bool>(), 2),
                prop::collection::vec((0i32..=9, 10i32..=19), 2),
                any::<bool>(),
            )
                .prop_map(
                    |(tmemory_flags, tmemory_bytes, object_flags, object_values, committed)| {
                        let tmemory_writes = tmemory_flags
                            .into_iter()
                            .zip(tmemory_bytes)
                            .enumerate()
                            .filter_map(|(granule, (selected, (old_byte, new_byte)))| {
                                selected.then_some(ModelTMemoryWrite {
                                    granule: u32::try_from(granule).unwrap(),
                                    old_byte,
                                    new_byte,
                                })
                            })
                            .collect();
                        let object_writes = object_flags
                            .into_iter()
                            .zip(object_values)
                            .enumerate()
                            .filter_map(|(object_id, (selected, (old_value, new_value)))| {
                                selected.then_some(ModelObjectWrite {
                                    object_id: u64::try_from(object_id).unwrap(),
                                    old_value,
                                    new_value,
                                })
                            })
                            .collect();
                        MixedModelCase {
                            tmemory_writes,
                            object_writes,
                            committed,
                        }
                    },
                )
        }

        fn run_mixed_model_case(case: &MixedModelCase) -> Result<ActualMixedWorld> {
            let outcome = (|| -> Result<ActualMixedWorld> {
                let dir = tempfile::tempdir()?;
                let tmemory_path = dir.path().join("tmemory.bin");
                let tx_log_path = dir.path().join("tx-log.bin");
                let mut tmemory = crate::runtime::vm::TMemory::new(
                    TransactionConfig::with_file_backed_tmemory_path(tmemory_path.clone()).unwrap(),
                    1,
                    Some(1),
                )?;

                for write in &case.tmemory_writes {
                    tmemory.commit_staged_tmemory_granule(
                        u64::from(write.granule),
                        &repeated_granule(write.old_byte),
                    )?;
                }

                let durable_log = TxDurableLog::create_file_backed(&tx_log_path, 64)?;
                let mut state = TransactionState::new_for_test_with_durable_log(
                    TransactionId::from_raw(71),
                    durable_log,
                );
                let mut objects = ObjectTable::default();
                let mut object_handles = Vec::new();
                for object_id in 0..2u64 {
                    let object = objects.allocate_persistent_struct_for_gc_ref(
                        u32::try_from(0x200 + object_id)
                            .context("object model gc ref does not fit u32")?,
                        vec![ObjectValue::I32(0)],
                    )?;
                    assert_eq!(object.object_index, object_id);
                    object_handles.push(object);
                }
                for write in &case.object_writes {
                    let handle = object_handles
                        .get(
                            usize::try_from(write.object_id)
                                .context("object model id does not fit host usize")?,
                        )
                        .copied()
                        .context("object model id is outside the prepared object slots")?;
                    objects.update_payload(
                        handle,
                        ObjectPayload::Struct(vec![ObjectValue::I32(write.old_value)]),
                    )?;
                }

                let mut final_marker = None;
                for write in &case.tmemory_writes {
                    let new_granule = repeated_granule(write.new_byte);
                    let marker = state.publish_tmemory_undo_before_in_place_write(
                        71,
                        71,
                        &tmemory.prepare_tmemory_undo_record(
                            Some(7),
                            0,
                            u64::from(write.granule),
                            &new_granule,
                        )?,
                    )?;
                    tmemory
                        .commit_staged_tmemory_granule(u64::from(write.granule), &new_granule)?;
                    final_marker = Some(marker);
                }

                for write in &case.object_writes {
                    let handle = object_handles
                        .get(
                            usize::try_from(write.object_id)
                                .context("object model id does not fit host usize")?,
                        )
                        .copied()
                        .context("object model id is outside the prepared object slots")?;
                    state.acquire_object_write(&objects, handle)?;
                    state.stage_struct_field(
                        &objects,
                        handle,
                        0,
                        ObjectValue::I32(write.new_value),
                    )?;
                }

                let mut publications = Vec::new();
                state.commit_object_payloads_into(&mut objects, &mut publications)?;
                if let Some(marker) =
                    state.publish_object_publications_before_commit(71, 71, &publications)?
                {
                    final_marker = Some(marker);
                }
                if case.committed {
                    state.publish_commit_lp(
                        71,
                        71,
                        final_marker.context(
                            "mixed model case must publish at least one tmemory or object record",
                        )?,
                    )?;
                }
                drop(state);

                let recovered = TxDurableLog::recover_file_backed_for_test(&tx_log_path)?;
                tmemory.apply_file_backed_recovered_tmemory_undo_rollbacks_for_test(
                    recovered.tmemory_undo_rollbacks,
                )?;
                let tmemory_file = std::fs::read(&tmemory_path)?;
                let mut world = ModelWorld::default();
                for write in &case.tmemory_writes {
                    world.tmemory_granules.insert(
                        write.granule,
                        file_backed_granule_byte(&tmemory_file, write.granule)?,
                    );
                }

                let object_winners =
                    crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
                        &tx_log_path,
                    )?;
                let object_winner_count = object_winners.len();
                let mut recovered_objects = ObjectTable::default();
                recovered_objects.rebuild_from_recovery_for_test(&object_winners)?;
                for winner in &object_winners {
                    let payload = recovered_objects.payload(ObjectId {
                        object_index: winner.object_id,
                    })?;
                    let ObjectPayload::Struct(fields) = payload else {
                        bail!("mixed model expected recovered object winner to be a struct");
                    };
                    let field = fields
                        .first()
                        .context("mixed model expected recovered object to have one field")?;
                    let ObjectValue::I32(value) = field else {
                        bail!("mixed model expected recovered object field to be i32");
                    };
                    world.object_fields.insert(winner.object_id, *value);
                }

                Ok(ActualMixedWorld {
                    world,
                    object_winner_count,
                })
            })();
            clear_current_thread_transaction_for_test();
            outcome
        }

        #[test]
        fn model_mixed_file_backed_commit_matches_reference_world() {
            let case = MixedModelCase {
                tmemory_writes: vec![ModelTMemoryWrite {
                    granule: 0,
                    old_byte: 0x11,
                    new_byte: 0x22,
                }],
                object_writes: vec![ModelObjectWrite {
                    object_id: 0,
                    old_value: 1,
                    new_value: 9,
                }],
                committed: true,
            };

            let actual = run_mixed_model_case(&case).unwrap();

            assert_eq!(actual.world, case.expected_world());
            assert_eq!(actual.world.tmemory_granules.get(&0), Some(&0x22));
            assert_eq!(actual.world.object_fields.get(&0), Some(&9));
            assert_eq!(actual.object_winner_count, 1);
        }

        #[test]
        fn model_mixed_file_backed_loose_end_matches_reference_world() {
            let case = MixedModelCase {
                tmemory_writes: vec![ModelTMemoryWrite {
                    granule: 0,
                    old_byte: 0x33,
                    new_byte: 0x44,
                }],
                object_writes: vec![ModelObjectWrite {
                    object_id: 0,
                    old_value: 1,
                    new_value: 9,
                }],
                committed: false,
            };

            let actual = run_mixed_model_case(&case).unwrap();

            assert_eq!(actual.world, case.expected_world());
            assert_eq!(actual.world.tmemory_granules.get(&0), Some(&0x33));
            assert!(actual.world.object_fields.is_empty());
            assert_eq!(actual.object_winner_count, 0);
        }

        #[test]
        fn model_mixed_file_backed_generated_histories_match_reference_world() {
            let mut runner = TestRunner::new(Config {
                cases: 48,
                failure_persistence: None,
                max_shrink_iters: 256,
                rng_seed: RngSeed::Fixed(0x5eed_0003),
                ..Config::default()
            });

            runner
                .run(&model_case_strategy(), |case| {
                    let actual = run_mixed_model_case(&case)
                        .map_err(|error| TestCaseError::fail(error.to_string()))?;
                    prop_assert_eq!(actual.world, case.expected_world());
                    if case.committed {
                        prop_assert_eq!(actual.object_winner_count, case.object_writes.len());
                    } else {
                        prop_assert_eq!(actual.object_winner_count, 0);
                    }
                    Ok(())
                })
                .unwrap();
        }
    }

    mod transaction_active {
        use super::*;
        use std::fmt::Debug;
        use std::sync::{Arc, Mutex};

        fn assert_requires_active<T: Debug>(result: Result<T>) {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("transaction operation requires an active transaction"),
                "{error:?}"
            );
        }

        fn observed_transaction_ids(observed: &Arc<Mutex<Vec<Option<u64>>>>) -> Vec<Option<u64>> {
            observed.lock().unwrap().clone()
        }

        fn assert_active_transaction_id(id: Option<u64>) {
            let Some(id) = id else {
                panic!("expected an active transaction id to be observed");
            };
            assert!(id > 0, "expected a nonzero active transaction id, got {id}");
        }

        fn observing_import(
            store: &mut crate::Store<()>,
            observed: Arc<Mutex<Vec<Option<u64>>>>,
        ) -> crate::Func {
            crate::Func::wrap(store, move || {
                observed
                    .lock()
                    .unwrap()
                    .push(current_thread_transaction_for_test().map(TransactionId::as_raw));
            })
        }

        #[test]
        fn direct_transactional_operations_do_not_succeed_without_active_transaction() {
            clear_current_thread_transaction_for_test();
            let mut state = TransactionState::default();
            let backing = vec![0; TMEMORY_GRANULE_SIZE];
            let mut objects = ObjectTable::default();
            let struct_object = objects
                .allocate_persistent_struct_for_gc_ref(0x701, vec![ObjectValue::I32(7)])
                .unwrap();
            let array_object = objects
                .allocate_persistent_array_for_gc_ref(0x702, vec![ObjectValue::I32(9)])
                .unwrap();

            assert_requires_active(state.acquire_memory_granule_read(0, 0));
            assert_requires_active(state.acquire_memory_granule_write(
                0,
                0,
                vec![0xaa; TMEMORY_GRANULE_SIZE],
            ));
            assert_requires_active(state.acquire_memory_size_read_owned(None, 0));
            assert_requires_active(state.acquire_memory_size_write_owned(None, 0));
            assert_requires_active(state.read_memory_overlay(0, 0, 1, &backing));
            assert_requires_active(state.stage_memory_write(0, 0, &[0xaa], &backing));

            assert_requires_active(state.acquire_global_read_owned(None, 0));
            assert_requires_active(state.acquire_global_write_owned(None, 0));
            assert_requires_active(state.stage_global(0, GlobalSnapshot::I32(11)));

            assert_requires_active(state.acquire_table_granule_read_owned(None, 0, 0, 0));
            assert_requires_active(state.acquire_table_granule_write_owned(None, 0, 0, 0));
            assert_requires_active(state.acquire_table_granule_read_range_owned(None, 0, 0, 1, 0));
            assert_requires_active(state.acquire_table_granule_write_range_owned(None, 0, 0, 1, 0));
            assert_requires_active(state.acquire_table_size_read_owned(None, 0, 0));
            assert_requires_active(state.acquire_table_size_write_owned(None, 0, 0));

            assert_requires_active(state.acquire_object_read(&objects, struct_object));
            assert_requires_active(state.acquire_object_write(&objects, struct_object));
            assert!(
                state.read_struct_field(&objects, struct_object, 0).is_err(),
                "persistent object read without an active transaction should not succeed"
            );
            assert!(
                state
                    .stage_struct_field(&objects, struct_object, 0, ObjectValue::I32(12))
                    .is_err(),
                "persistent object write without an active transaction should not succeed"
            );
            assert!(
                state.read_array_element(&objects, array_object, 0).is_err(),
                "persistent array read without an active transaction should not succeed"
            );
            assert!(
                state
                    .stage_array_element(&objects, array_object, 0, ObjectValue::I32(12))
                    .is_err(),
                "persistent array write without an active transaction should not succeed"
            );
            assert_eq!(current_thread_transaction_for_test(), None);
        }

        #[test]
        fn exported_tfunc_starts_and_clears_transaction_state() {
            clear_current_thread_transaction_for_test();
            let engine = crate::Engine::default();
            let module = transaction_test_module(
                &engine,
                r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc (export "first")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 1)))
                  (tfunc (export "second")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 2)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
            );
            let mut store = crate::Store::new(&engine, ());
            let observed = Arc::new(Mutex::new(Vec::new()));
            let observe = observing_import(&mut store, Arc::clone(&observed));
            let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
            let first = instance
                .get_typed_func::<(), ()>(&mut store, "first")
                .unwrap();
            let second = instance
                .get_typed_func::<(), ()>(&mut store, "second")
                .unwrap();
            let read = instance
                .get_typed_func::<(), i32>(&mut store, "read")
                .unwrap();

            assert_eq!(current_thread_transaction_for_test(), None);
            first.call(&mut store, ()).unwrap();
            assert_eq!(read.call(&mut store, ()).unwrap(), 1);
            assert_eq!(current_thread_transaction_for_test(), None);

            second.call(&mut store, ()).unwrap();
            assert_eq!(read.call(&mut store, ()).unwrap(), 2);
            assert_eq!(current_thread_transaction_for_test(), None);

            let observed = observed_transaction_ids(&observed);
            assert_eq!(observed.len(), 2);
            assert_active_transaction_id(observed[0]);
            assert_active_transaction_id(observed[1]);
            assert_ne!(observed[0], observed[1]);
        }

        #[test]
        fn trap_path_aborts_and_clears_transaction_state() {
            clear_current_thread_transaction_for_test();
            let engine = crate::Engine::default();
            let module = transaction_test_module(
                &engine,
                r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc (export "trap")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 42))
                    (unreachable))
                  (tfunc (export "write")
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 7)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
            );
            let mut store = crate::Store::new(&engine, ());
            let observed = Arc::new(Mutex::new(Vec::new()));
            let observe = observing_import(&mut store, Arc::clone(&observed));
            let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
            let trap = instance
                .get_typed_func::<(), ()>(&mut store, "trap")
                .unwrap();
            let write = instance
                .get_typed_func::<(), ()>(&mut store, "write")
                .unwrap();
            let read = instance
                .get_typed_func::<(), i32>(&mut store, "read")
                .unwrap();

            assert!(trap.call(&mut store, ()).is_err());
            assert_eq!(current_thread_transaction_for_test(), None);
            assert_eq!(read.call(&mut store, ()).unwrap(), 0);

            write.call(&mut store, ()).unwrap();
            assert_eq!(current_thread_transaction_for_test(), None);
            assert_eq!(read.call(&mut store, ()).unwrap(), 7);

            let observed = observed_transaction_ids(&observed);
            assert_eq!(observed.len(), 2);
            assert_active_transaction_id(observed[0]);
            assert_active_transaction_id(observed[1]);
            assert_ne!(observed[0], observed[1]);
        }

        #[test]
        fn nested_tcall_reuses_active_transaction_until_outer_return() {
            clear_current_thread_transaction_for_test();
            let engine = crate::Engine::default();
            let module = transaction_test_module(
                &engine,
                r#"
                (module
                  (import "env" "observe" (func $observe))
                  (tmemory 1)
                  (tfunc $inner
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 1)))
                  (tfunc (export "outer")
                    (call $observe)
                    (tcall $inner)
                    (call $observe)
                    (i32.tstore (i32.const 0) (i32.const 2)))
                  (tfunc (export "read") (result i32)
                    (i32.tload (i32.const 0))))
                "#,
            );
            let mut store = crate::Store::new(&engine, ());
            let observed = Arc::new(Mutex::new(Vec::new()));
            let observe = observing_import(&mut store, Arc::clone(&observed));
            let instance = crate::Instance::new(&mut store, &module, &[observe.into()]).unwrap();
            let outer = instance
                .get_typed_func::<(), ()>(&mut store, "outer")
                .unwrap();
            let read = instance
                .get_typed_func::<(), i32>(&mut store, "read")
                .unwrap();

            outer.call(&mut store, ()).unwrap();

            assert_eq!(read.call(&mut store, ()).unwrap(), 2);
            assert_eq!(current_thread_transaction_for_test(), None);

            let observed = observed_transaction_ids(&observed);
            assert_eq!(observed.len(), 3);
            assert!(observed.iter().all(Option::is_some));
            assert_eq!(observed[0], observed[1]);
            assert_eq!(observed[1], observed[2]);
        }
    }

    mod model_lock_based {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

        const MODEL_LOCK_BASED_EXHAUSTIVE_ENV: &str = "WASMTIME_TRANSACTION_LOCK_MODEL_EXHAUSTIVE";
        const MODEL_LOCK_BASED_DEFAULT_MAX_EXHAUSTIVE_LEN: usize = 3;
        const MODEL_LOCK_BASED_DEFAULT_EXHAUSTIVE_SCHEDULES: usize = 8_421;
        const MODEL_LOCK_BASED_FULL_MAX_EXHAUSTIVE_LEN: usize = 5;
        const MODEL_LOCK_BASED_FULL_EXHAUSTIVE_SCHEDULES: usize = 3_368_421;

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum LockModelOp {
            Read { tx: u64, granule: u8, version: u64 },
            Write { tx: u64, granule: u8, version: u64 },
            Abort { tx: u64 },
            Release { tx: u64 },
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum LockModelErrorKind {
            ReadOwnedByOther,
            ReadVersionMismatch,
            WriteOwnedByOther,
            WriteVersionMismatch,
        }

        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        struct LockModelStep {
            error_kind: Option<LockModelErrorKind>,
            released_owner: Option<u64>,
        }

        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        struct LockModelState {
            owners: BTreeMap<u8, u64>,
            owner_sets: BTreeMap<u64, BTreeSet<u8>>,
            read_versions: BTreeMap<(u64, u8), u64>,
        }

        impl LockModelErrorKind {
            fn as_actual(self) -> LockBasedConflictKindForTest {
                match self {
                    Self::ReadOwnedByOther => LockBasedConflictKindForTest::ReadOwnedByOther,
                    Self::ReadVersionMismatch => LockBasedConflictKindForTest::ReadVersionMismatch,
                    Self::WriteOwnedByOther => LockBasedConflictKindForTest::WriteOwnedByOther,
                    Self::WriteVersionMismatch => {
                        LockBasedConflictKindForTest::WriteVersionMismatch
                    }
                }
            }
        }

        impl LockModelState {
            fn apply(&mut self, op: LockModelOp) -> LockModelStep {
                match op {
                    LockModelOp::Read {
                        tx,
                        granule,
                        version,
                    } => self.record_read(tx, granule, version),
                    LockModelOp::Write {
                        tx,
                        granule,
                        version,
                    } => self.acquire_write(tx, granule, version),
                    LockModelOp::Abort { tx } | LockModelOp::Release { tx } => {
                        self.release_transaction(tx);
                        LockModelStep::default()
                    }
                }
            }

            fn record_read(&mut self, tx: u64, granule: u8, version: u64) -> LockModelStep {
                let mut step = LockModelStep::default();
                match self.resolve_writer_conflict(tx, granule, false) {
                    Ok(released_owner) => step.released_owner = released_owner,
                    Err(error_kind) => {
                        step.error_kind = Some(error_kind);
                        return step;
                    }
                }

                match self.read_versions.entry((tx, granule)) {
                    alloc::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(version);
                    }
                    alloc::collections::btree_map::Entry::Occupied(entry) => {
                        if *entry.get() != version {
                            step.error_kind = Some(LockModelErrorKind::ReadVersionMismatch);
                        }
                    }
                }

                step
            }

            fn acquire_write(&mut self, tx: u64, granule: u8, version: u64) -> LockModelStep {
                let mut step = LockModelStep::default();
                match self.resolve_writer_conflict(tx, granule, true) {
                    Ok(released_owner) => step.released_owner = released_owner,
                    Err(error_kind) => {
                        step.error_kind = Some(error_kind);
                        return step;
                    }
                }

                if self
                    .read_versions
                    .get(&(tx, granule))
                    .is_some_and(|current| *current != version)
                {
                    step.error_kind = Some(LockModelErrorKind::WriteVersionMismatch);
                    return step;
                }

                if let Some(previous_owner) = self.owners.insert(granule, tx) {
                    if previous_owner != tx {
                        self.owner_sets
                            .get_mut(&previous_owner)
                            .unwrap()
                            .remove(&granule);
                        if self
                            .owner_sets
                            .get(&previous_owner)
                            .is_some_and(BTreeSet::is_empty)
                        {
                            self.owner_sets.remove(&previous_owner);
                        }
                    }
                }
                self.owner_sets.entry(tx).or_default().insert(granule);
                step
            }

            fn resolve_writer_conflict(
                &mut self,
                tx: u64,
                granule: u8,
                is_write: bool,
            ) -> core::result::Result<Option<u64>, LockModelErrorKind> {
                let Some(owner) = self.owners.get(&granule).copied() else {
                    return Ok(None);
                };
                if owner == tx {
                    return Ok(None);
                }
                if tx < owner {
                    self.release_transaction(owner);
                    return Ok(Some(owner));
                }
                Err(if is_write {
                    LockModelErrorKind::WriteOwnedByOther
                } else {
                    LockModelErrorKind::ReadOwnedByOther
                })
            }

            fn release_transaction(&mut self, tx: u64) {
                if let Some(granules) = self.owner_sets.remove(&tx) {
                    for granule in granules {
                        self.owners.remove(&granule);
                    }
                }
                self.read_versions.retain(|(reader, _), _| *reader != tx);
            }

            fn assert_internal_invariants(&self) -> Result<()> {
                let owner_count: usize = self.owner_sets.values().map(BTreeSet::len).sum();
                ensure!(
                    owner_count == self.owners.len(),
                    "owner-set size {owner_count} does not match owner map size {}",
                    self.owners.len()
                );
                for (&granule, &owner) in &self.owners {
                    ensure!(
                        self.owner_sets
                            .get(&owner)
                            .is_some_and(|granules| granules.contains(&granule)),
                        "owner map says tx {owner} owns granule {granule}, but owner set disagrees"
                    );
                }
                for (&tx, granules) in &self.owner_sets {
                    ensure!(
                        !granules.is_empty(),
                        "owner set for tx {tx} should not be empty"
                    );
                    for &granule in granules {
                        ensure!(
                            self.owners.get(&granule) == Some(&tx),
                            "owner set says tx {tx} owns granule {granule}, but owner map disagrees"
                        );
                    }
                }
                Ok(())
            }
        }

        fn model_tx(tx: u64) -> TransactionId {
            TransactionId::from_raw(tx)
        }

        fn model_granule(granule: u8) -> GranuleId {
            GranuleId::TMemory {
                instance: Some(1),
                memory_index: 0,
                granule_index: u64::from(granule),
            }
        }

        fn decode_model_granule(granule: GranuleId) -> Result<u8> {
            let GranuleId::TMemory {
                instance: Some(1),
                memory_index: 0,
                granule_index,
            } = granule
            else {
                bail!("unexpected lock model granule in snapshot: {granule:?}");
            };
            let granule =
                u8::try_from(granule_index).context("lock model granule index does not fit u8")?;
            ensure!(
                granule <= 1,
                "lock model granule index {granule} is outside the Wave 5 domain"
            );
            Ok(granule)
        }

        fn snapshot_lock_state(snapshot: &LockBasedSnapshotForTest) -> Result<LockModelState> {
            let mut state = LockModelState::default();
            for (&granule, &owner) in &snapshot.owners {
                let granule = decode_model_granule(granule)?;
                let owner = owner.as_raw();
                state.owners.insert(granule, owner);
                state.owner_sets.entry(owner).or_default().insert(granule);
            }
            for (&(reader, granule), &version) in &snapshot.read_versions {
                let reader = reader.as_raw();
                let granule = decode_model_granule(granule)?;
                state.read_versions.insert((reader, granule), version);
            }
            state.assert_internal_invariants()?;
            Ok(state)
        }

        fn apply_lock_model_op(
            locks: &mut LockBased,
            op: LockModelOp,
        ) -> Option<LockBasedConflictKindForTest> {
            match op {
                LockModelOp::Read {
                    tx,
                    granule,
                    version,
                } => locks
                    .record_read_result_for_test(model_tx(tx), model_granule(granule), version)
                    .err()
                    .map(|error| error),
                LockModelOp::Write {
                    tx,
                    granule,
                    version,
                } => locks
                    .acquire_write_result_for_test(model_tx(tx), model_granule(granule), version)
                    .err()
                    .map(|error| error),
                LockModelOp::Abort { tx } => {
                    locks.abort_for_test(model_tx(tx));
                    None
                }
                LockModelOp::Release { tx } => {
                    locks.release_transaction(model_tx(tx));
                    None
                }
            }
        }

        fn assert_lock_step(
            prefix: &[LockModelOp],
            op: LockModelOp,
            before: &LockModelState,
            after: &LockModelState,
            step: LockModelStep,
            actual_error: Option<LockBasedConflictKindForTest>,
        ) -> Result<()> {
            if let Some(error_kind) = step.error_kind {
                let actual_error = actual_error
                    .context("real LockBased op succeeded when model expected error")?;
                ensure!(
                    actual_error == error_kind.as_actual(),
                    "schedule prefix {prefix:?} expected error {:?}, got {:?}",
                    error_kind.as_actual(),
                    actual_error
                );
            } else {
                ensure!(
                    actual_error.is_none(),
                    "schedule prefix {prefix:?} unexpectedly failed with {actual_error:?}"
                );
            }

            if let Some(released_owner) = step.released_owner {
                ensure!(
                    !after.owner_sets.contains_key(&released_owner),
                    "schedule prefix {prefix:?} should release tx {released_owner} ownership"
                );
                ensure!(
                    after.owners.values().all(|owner| *owner != released_owner),
                    "schedule prefix {prefix:?} still shows tx {released_owner} in owner map"
                );
                ensure!(
                    after
                        .read_versions
                        .keys()
                        .all(|(reader, _)| *reader != released_owner),
                    "schedule prefix {prefix:?} still shows tx {released_owner} in read set"
                );
            }

            match op {
                LockModelOp::Abort { tx } | LockModelOp::Release { tx } => {
                    ensure!(
                        !after.owner_sets.contains_key(&tx),
                        "schedule prefix {prefix:?} should clear owner set for tx {tx}"
                    );
                    ensure!(
                        after.owners.values().all(|owner| *owner != tx),
                        "schedule prefix {prefix:?} should clear owner map entries for tx {tx}"
                    );
                    ensure!(
                        after.read_versions.keys().all(|(reader, _)| *reader != tx),
                        "schedule prefix {prefix:?} should clear read versions for tx {tx}"
                    );
                }
                LockModelOp::Read { tx, granule, .. } => {
                    if step.error_kind == Some(LockModelErrorKind::ReadOwnedByOther) {
                        ensure!(
                            after.owners.get(&granule) == before.owners.get(&granule),
                            "schedule prefix {prefix:?} should not change owner on failed read conflict"
                        );
                    }
                    if step.error_kind == Some(LockModelErrorKind::ReadVersionMismatch) {
                        ensure!(
                            before.read_versions.get(&(tx, granule))
                                == after.read_versions.get(&(tx, granule)),
                            "schedule prefix {prefix:?} should preserve the original read version on mismatch"
                        );
                    }
                }
                LockModelOp::Write { granule, .. } => {
                    if step.error_kind == Some(LockModelErrorKind::WriteOwnedByOther) {
                        ensure!(
                            after.owners.get(&granule) == before.owners.get(&granule),
                            "schedule prefix {prefix:?} should not steal ownership on failed write conflict"
                        );
                    }
                }
            }

            Ok(())
        }

        fn run_lock_model_schedule(schedule: &[LockModelOp]) -> Result<LockModelState> {
            let mut locks = LockBased::default();
            let mut model = LockModelState::default();
            let mut prefix = Vec::with_capacity(schedule.len());

            let actual_initial = snapshot_lock_state(&locks.snapshot_for_test())?;
            ensure!(
                actual_initial == model,
                "initial lock state diverged before running any schedule"
            );

            for &op in schedule {
                prefix.push(op);
                let before = model.clone();
                let step = model.apply(op);
                let actual_error = apply_lock_model_op(&mut locks, op);
                let actual = snapshot_lock_state(&locks.snapshot_for_test())?;
                model.assert_internal_invariants()?;
                ensure!(
                    actual == model,
                    "schedule prefix {prefix:?} diverged\nexpected: {model:?}\nactual:   {actual:?}"
                );
                assert_lock_step(&prefix, op, &before, &model, step, actual_error)?;
            }

            Ok(model)
        }

        fn exhaustive_lock_model_alphabet() -> Vec<LockModelOp> {
            let mut ops = Vec::new();
            for tx in [1, 2] {
                for granule in [0, 1] {
                    for version in [0, 1] {
                        ops.push(LockModelOp::Read {
                            tx,
                            granule,
                            version,
                        });
                        ops.push(LockModelOp::Write {
                            tx,
                            granule,
                            version,
                        });
                    }
                }
                ops.push(LockModelOp::Abort { tx });
                ops.push(LockModelOp::Release { tx });
            }
            ops
        }

        fn run_exhaustive_lock_model_schedules(max_len: usize) -> Result<usize> {
            fn visit(
                prefix: &mut Vec<LockModelOp>,
                real: &LockBased,
                model: &LockModelState,
                alphabet: &[LockModelOp],
                remaining: usize,
            ) -> Result<usize> {
                let mut schedule_count = 1;
                if remaining == 0 {
                    return Ok(schedule_count);
                }

                for &op in alphabet {
                    prefix.push(op);
                    let mut next_real = real.clone_for_test();
                    let mut next_model = model.clone();
                    let step = next_model.apply(op);
                    let actual_error = apply_lock_model_op(&mut next_real, op);
                    let actual = snapshot_lock_state(&next_real.snapshot_for_test())?;
                    next_model.assert_internal_invariants()?;
                    ensure!(
                        actual == next_model,
                        "schedule prefix {prefix:?} diverged\nexpected: {next_model:?}\nactual:   {actual:?}"
                    );
                    let before = model.clone();
                    assert_lock_step(prefix, op, &before, &next_model, step, actual_error)?;
                    schedule_count +=
                        visit(prefix, &next_real, &next_model, alphabet, remaining - 1)?;
                    prefix.pop();
                }

                Ok(schedule_count)
            }

            let alphabet = exhaustive_lock_model_alphabet();
            let mut prefix = Vec::new();
            let real = LockBased::default();
            let model = LockModelState::default();
            visit(&mut prefix, &real, &model, &alphabet, max_len)
        }

        fn lock_model_op_strategy() -> impl Strategy<Value = LockModelOp> {
            prop_oneof![
                (1u64..=2, 0u8..=1, 0u64..=1).prop_map(|(tx, granule, version)| {
                    LockModelOp::Read {
                        tx,
                        granule,
                        version,
                    }
                }),
                (1u64..=2, 0u8..=1, 0u64..=1).prop_map(|(tx, granule, version)| {
                    LockModelOp::Write {
                        tx,
                        granule,
                        version,
                    }
                }),
                (1u64..=2).prop_map(|tx| LockModelOp::Abort { tx }),
                (1u64..=2).prop_map(|tx| LockModelOp::Release { tx }),
            ]
        }

        #[test]
        fn model_lock_based_deterministic_schedule_matches_reference_world() {
            let schedule = vec![
                LockModelOp::Write {
                    tx: 2,
                    granule: 0,
                    version: 0,
                },
                LockModelOp::Read {
                    tx: 1,
                    granule: 0,
                    version: 0,
                },
                LockModelOp::Write {
                    tx: 1,
                    granule: 0,
                    version: 0,
                },
                LockModelOp::Write {
                    tx: 1,
                    granule: 1,
                    version: 1,
                },
                LockModelOp::Release { tx: 1 },
            ];

            let outcome = run_lock_model_schedule(&schedule).unwrap();

            assert!(outcome.owners.is_empty());
            assert!(outcome.owner_sets.is_empty());
            assert!(outcome.read_versions.is_empty());
        }

        #[test]
        fn model_lock_based_exhaustive_small_schedules_match_reference_world() {
            // Default runs stay quick by covering the full alphabet through length 3.
            // Opt in to the full depth-5 enumeration when explicitly requested.
            let full_exhaustive =
                std::env::var(MODEL_LOCK_BASED_EXHAUSTIVE_ENV).is_ok_and(|value| value == "1");
            let (max_len, expected_schedule_count) = if full_exhaustive {
                (
                    MODEL_LOCK_BASED_FULL_MAX_EXHAUSTIVE_LEN,
                    MODEL_LOCK_BASED_FULL_EXHAUSTIVE_SCHEDULES,
                )
            } else {
                (
                    MODEL_LOCK_BASED_DEFAULT_MAX_EXHAUSTIVE_LEN,
                    MODEL_LOCK_BASED_DEFAULT_EXHAUSTIVE_SCHEDULES,
                )
            };

            let schedule_count = run_exhaustive_lock_model_schedules(max_len).unwrap();

            assert_eq!(schedule_count, expected_schedule_count);
        }

        #[test]
        fn model_lock_based_generated_schedules_match_reference_world() {
            // Exhaustive coverage already owns the tiny prefix space. These fixed-seed
            // profiles spend the same budget on longer churn histories instead.
            for (cases, seed, len_range) in [
                (64, 0x5eed_0005, 4..=12usize),
                (32, 0x5eed_0505, 24..=48usize),
            ] {
                let mut runner = TestRunner::new(Config {
                    cases,
                    failure_persistence: None,
                    max_shrink_iters: 256,
                    rng_seed: RngSeed::Fixed(seed),
                    ..Config::default()
                });

                runner
                    .run(
                        &prop::collection::vec(lock_model_op_strategy(), len_range),
                        |schedule| {
                            run_lock_model_schedule(&schedule)
                                .map_err(|error| TestCaseError::fail(error.to_string()))?;
                            Ok(())
                        },
                    )
                    .unwrap();
            }
        }
    }

    mod model_object_rebuild {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct ModelObjectRecord {
            object_id: u64,
            version: u32,
            kind: u16,
            field: i32,
            committed: bool,
        }

        #[derive(Clone, Copy, Debug)]
        struct ModelObjectSeed {
            object_id: u64,
            field: i32,
            committed: bool,
        }

        #[derive(Debug)]
        struct RecoveredObjectHistory {
            rebuilt: ObjectTable,
            recovered_region:
                crate::runtime::vm::block_region::TransactionPersistenceRecoveredRegion,
            object_winners: Vec<crate::runtime::vm::RecoveredObjectWinner>,
        }

        fn model_kind_strategy() -> impl Strategy<Value = u16> {
            prop_oneof![
                Just(ObjectKind::Struct as u16),
                Just(ObjectKind::Array as u16),
            ]
        }

        fn model_object_history_strategy() -> impl Strategy<Value = Vec<ModelObjectRecord>> {
            prop::collection::vec(model_kind_strategy(), 4).prop_flat_map(|kinds| {
                prop::collection::vec(
                    (0u64..=3u64, -20i32..=20i32, any::<bool>()).prop_map(
                        |(object_id, field, committed)| ModelObjectSeed {
                            object_id,
                            field,
                            committed,
                        },
                    ),
                    1..=8usize,
                )
                .prop_map(move |seeds| {
                    let mut versions = BTreeMap::<u64, u32>::new();
                    seeds
                        .into_iter()
                        .map(|seed| {
                            let next_version = versions
                                .entry(seed.object_id)
                                .and_modify(|version| *version += 1)
                                .or_insert(1);
                            let kind_index = usize::try_from(seed.object_id).unwrap();
                            ModelObjectRecord {
                                object_id: seed.object_id,
                                version: *next_version,
                                kind: kinds[kind_index],
                                field: seed.field,
                                committed: seed.committed,
                            }
                        })
                        .collect()
                })
            })
        }

        fn model_payload(record: &ModelObjectRecord) -> Result<ObjectPayload> {
            Ok(match object_kind_from_u16(record.kind)? {
                ObjectKind::Struct => ObjectPayload::Struct(vec![ObjectValue::I32(record.field)]),
                ObjectKind::Array => ObjectPayload::Array(vec![ObjectValue::I32(record.field)]),
                kind => bail!("unsupported model object kind for rebuild test: {kind:?}"),
            })
        }

        fn model_domain(
            record: &ModelObjectRecord,
        ) -> Result<crate::runtime::vm::PackedGranuleDomain> {
            Ok(match object_kind_from_u16(record.kind)? {
                ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
                ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
                kind => bail!("unsupported model object kind for durable publication: {kind:?}"),
            })
        }

        fn model_type_index(record: &ModelObjectRecord) -> Result<u32> {
            u32::try_from(record.object_id)
                .context("model object id does not fit u32 for type index")
                .map(|id| 100 + id)
        }

        fn model_record_bytes(record: &ModelObjectRecord) -> Result<Vec<u8>> {
            encode_object_record_for_test(
                record.object_id,
                record.version,
                model_type_index(record)?,
                &model_payload(record)?,
            )
        }

        fn model_publication(record: &ModelObjectRecord) -> Result<persist::PendingPublication> {
            persist::PendingPublication::persistent_object(
                model_domain(record)?,
                record.object_id,
                record.version,
                model_type_index(record)?,
                model_record_bytes(record)?,
            )
        }

        fn expected_latest_committed_by_object(
            history: &[ModelObjectRecord],
        ) -> BTreeMap<u64, ModelObjectRecord> {
            let mut latest = BTreeMap::<u64, ModelObjectRecord>::new();
            let mut committed_versions = BTreeSet::<(u64, u32)>::new();
            for &record in history {
                if !record.committed {
                    continue;
                }
                assert!(
                    committed_versions.insert((record.object_id, record.version)),
                    "generated model history should not contain duplicate committed object/version pairs"
                );
                match latest.get(&record.object_id).copied() {
                    Some(current) if current.version > record.version => {}
                    Some(current) if current.version == record.version => unreachable!(),
                    _ => {
                        latest.insert(record.object_id, record);
                    }
                }
            }
            latest
        }

        fn rebuild_object_table_from_winners(
            winners: &[crate::runtime::vm::RecoveredObjectWinner],
        ) -> Result<ObjectTable> {
            let mut objects = ObjectTable::default();
            objects.rebuild_from_recovery_for_test(winners)?;
            Ok(objects)
        }

        fn recover_object_history(history: &[ModelObjectRecord]) -> Result<RecoveredObjectHistory> {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("tx-log.bin");
            let mut log = TxDurableLog::create_file_backed(&path, 64)?;

            for (index, record) in history.iter().enumerate() {
                let stream_id =
                    u32::try_from(index.checked_add(1).context(
                        "model object history index overflow while assigning stream id",
                    )?)
                    .context("model object history stream id does not fit u32")?;
                let txid = stream_id
                    .checked_add(400)
                    .context("model object history txid overflow")?;
                let publication = model_publication(record)?;
                let marker = {
                    let mut sink = log.stream_sink(stream_id);
                    let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
                    publisher.publish_object_publication_before_commit(&publication)?
                };
                if record.committed {
                    let mut sink = log.stream_sink(stream_id);
                    let mut publisher = StreamPublisher::new(&mut sink, stream_id, txid);
                    publisher.publish_commit_lp(marker)?;
                }
            }
            drop(log);

            let recovered_region = TxDurableLog::recover_file_backed_for_test(&path)?;
            let object_winners =
                crate::runtime::vm::block_region::reopen_and_recover_file_backed_object_winners_for_test(
                    &path,
                )?;
            let mut rebuilt = ObjectTable::default();
            rebuilt.rebuild_from_recovery_for_test(&object_winners)?;

            Ok(RecoveredObjectHistory {
                rebuilt,
                recovered_region,
                object_winners,
            })
        }

        fn recover_rebuilt_object_history(history: &[ModelObjectRecord]) -> Result<ObjectTable> {
            Ok(recover_object_history(history)?.rebuilt)
        }

        fn expected_logical_id(record: &ModelObjectRecord) -> Result<u64> {
            crate::runtime::vm::pack_object_granule_id(model_domain(record)?, record.object_id)
        }

        fn assert_model_object_rebuild_matches_real_recovery(
            history: &[ModelObjectRecord],
        ) -> Result<()> {
            let expected = expected_latest_committed_by_object(history);
            let recovered = recover_object_history(history)?;
            let rebuilt = &recovered.rebuilt;
            let recovered_versions = recovered
                .recovered_region
                .object_winners
                .iter()
                .map(|winner| (winner.object_id, winner.version))
                .collect::<BTreeMap<_, _>>();

            ensure!(
                rebuilt.live_count() == expected.len(),
                "rebuilt object count mismatch\nexpected: {}\nactual:   {}",
                expected.len(),
                rebuilt.live_count()
            );

            for (&object_id, record) in &expected {
                let object = ObjectId {
                    object_index: object_id,
                };
                ensure!(
                    rebuilt.kind(object)? == object_kind_from_u16(record.kind)?,
                    "rebuilt kind mismatch for object {object_id}"
                );
                ensure!(
                    rebuilt.is_persistent(object)?,
                    "rebuilt object {object_id} should stay persistent"
                );
                ensure!(
                    rebuilt.payload(object)? == model_payload(record)?,
                    "rebuilt payload mismatch for object {object_id}"
                );
                ensure!(
                    rebuilt.pending_publication_for_test(object)?.logical_id
                        == expected_logical_id(record)?,
                    "rebuilt logical id mismatch for object {object_id}"
                );
                ensure!(
                    rebuilt.pending_publication_for_test(object)?.version == record.version,
                    "rebuilt publication version mismatch for object {object_id}"
                );
                ensure!(
                    rebuilt.pending_publication_for_test(object)?.type_info
                        == model_type_index(record)?,
                    "rebuilt publication type info mismatch for object {object_id}"
                );
                ensure!(
                    rebuilt.trace_object_ids(object)?.is_empty(),
                    "model payloads should not carry object references"
                );
                ensure!(
                    recovered_versions.get(&object_id) == Some(&record.version),
                    "recovered summary version mismatch for object {object_id}"
                );
            }

            for record in history {
                if expected.contains_key(&record.object_id) {
                    continue;
                }
                ensure!(
                    rebuilt
                        .kind(ObjectId {
                            object_index: record.object_id,
                        })
                        .is_err(),
                    "loose-end object {} should not rebuild into a live slot",
                    record.object_id
                );
            }

            ensure!(
                recovered.object_winners.len() == expected.len(),
                "recovered object winner count mismatch\nexpected: {}\nactual:   {}",
                expected.len(),
                recovered.object_winners.len()
            );

            Ok(())
        }

        fn corrupt_payload_header_winner() -> crate::runtime::vm::RecoveredObjectWinner {
            let payload = ObjectPayload::Array(vec![ObjectValue::I32(9)]);
            let mut record_bytes = encode_object_record_for_test(7, 3, 207, &payload).unwrap();
            record_bytes[20..22].copy_from_slice(&(ObjectKind::Struct as u16).to_le_bytes());

            crate::runtime::vm::RecoveredObjectWinner {
                object_id: 7,
                version: 3,
                kind: ObjectKind::Struct as u16,
                type_index: 207,
                record_bytes,
            }
        }

        #[test]
        fn model_object_rebuild_latest_committed_version_wins() {
            let rebuilt = recover_rebuilt_object_history(&[
                ModelObjectRecord {
                    object_id: 41,
                    version: 2,
                    kind: ObjectKind::Struct as u16,
                    field: 20,
                    committed: true,
                },
                ModelObjectRecord {
                    object_id: 41,
                    version: 9,
                    kind: ObjectKind::Struct as u16,
                    field: 90,
                    committed: true,
                },
                ModelObjectRecord {
                    object_id: 41,
                    version: 7,
                    kind: ObjectKind::Struct as u16,
                    field: 70,
                    committed: true,
                },
            ])
            .unwrap();

            assert_eq!(rebuilt.live_count(), 1);
            assert!(
                rebuilt
                    .is_persistent(ObjectId { object_index: 41 })
                    .unwrap()
            );
            assert_eq!(
                rebuilt.payload(ObjectId { object_index: 41 }).unwrap(),
                ObjectPayload::Struct(vec![ObjectValue::I32(90)])
            );
            assert_eq!(
                rebuilt
                    .pending_publication_for_test(ObjectId { object_index: 41 })
                    .unwrap()
                    .version,
                9
            );
            assert_eq!(
                rebuilt
                    .pending_publication_for_test(ObjectId { object_index: 41 })
                    .unwrap()
                    .type_info,
                141
            );
        }

        #[test]
        fn model_object_rebuild_ignores_loose_end_publication() {
            let rebuilt = recover_rebuilt_object_history(&[
                ModelObjectRecord {
                    object_id: 52,
                    version: 1,
                    kind: ObjectKind::Struct as u16,
                    field: 10,
                    committed: true,
                },
                ModelObjectRecord {
                    object_id: 52,
                    version: 3,
                    kind: ObjectKind::Struct as u16,
                    field: 30,
                    committed: false,
                },
            ])
            .unwrap();

            assert_eq!(rebuilt.live_count(), 1);
            assert_eq!(
                rebuilt.payload(ObjectId { object_index: 52 }).unwrap(),
                ObjectPayload::Struct(vec![ObjectValue::I32(10)])
            );
        }

        #[test]
        fn model_object_rebuild_preserves_recovered_object_identity() {
            let rebuilt = recover_rebuilt_object_history(&[
                ModelObjectRecord {
                    object_id: 77,
                    version: 4,
                    kind: ObjectKind::Struct as u16,
                    field: 17,
                    committed: true,
                },
                ModelObjectRecord {
                    object_id: 103,
                    version: 6,
                    kind: ObjectKind::Struct as u16,
                    field: 88,
                    committed: true,
                },
            ])
            .unwrap();

            let object = ObjectId { object_index: 103 };
            assert_eq!(rebuilt.kind(object).unwrap(), ObjectKind::Struct);
            assert_eq!(
                rebuilt.payload(object).unwrap(),
                ObjectPayload::Struct(vec![ObjectValue::I32(88)])
            );
            assert_eq!(
                rebuilt
                    .pending_publication_for_test(object)
                    .unwrap()
                    .logical_id,
                crate::runtime::vm::pack_object_granule_id(
                    crate::runtime::vm::PackedGranuleDomain::TStruct,
                    103,
                )
                .unwrap()
            );
            assert_eq!(
                rebuilt
                    .pending_publication_for_test(object)
                    .unwrap()
                    .type_info,
                203
            );
        }

        #[test]
        fn model_object_rebuild_rejects_corrupt_payload_header_mismatch() {
            let err =
                rebuild_object_table_from_winners(&[corrupt_payload_header_winner()]).unwrap_err();

            assert!(
                err.to_string().contains(
                    "serialized struct payload length is not a multiple of object ABI size"
                )
            );
        }

        #[test]
        fn model_object_rebuild_rejects_duplicate_committed_same_version() {
            let err = recover_object_history(&[
                ModelObjectRecord {
                    object_id: 9,
                    version: 4,
                    kind: ObjectKind::Struct as u16,
                    field: 11,
                    committed: true,
                },
                ModelObjectRecord {
                    object_id: 9,
                    version: 4,
                    kind: ObjectKind::Struct as u16,
                    field: 22,
                    committed: true,
                },
            ])
            .unwrap_err();

            assert!(
                err.to_string()
                    .contains("duplicate committed object version 4 for object id 9")
            );
        }

        #[test]
        fn model_object_rebuild_generated_histories_match_real_recovery_and_reference_model() {
            let mut runner = TestRunner::new(Config {
                cases: 64,
                failure_persistence: None,
                max_shrink_iters: 256,
                rng_seed: RngSeed::Fixed(0x5eed_0009),
                ..Config::default()
            });

            runner
                .run(&model_object_history_strategy(), |history| {
                    assert_model_object_rebuild_matches_real_recovery(&history)
                        .map_err(|error| TestCaseError::fail(error.to_string()))
                })
                .unwrap();
        }
    }

    mod model_permissions {
        use super::*;
        use proptest::prelude::*;
        use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum PermissionModelOp {
            Begin,
            GrantRead,
            GrantWrite,
            Read,
            Write(i32),
            Downgrade,
            Abort,
            CommitRelease,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        enum PermissionStepOutcome {
            Bool { ok: bool, value: Option<bool> },
            Read { ok: bool, value: Option<i32> },
            Write { ok: bool },
            Unit { ok: bool },
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct PermissionRuntimeSnapshot {
            active: bool,
            committed_value: i32,
            staged_value: Option<i32>,
            read: bool,
            write: bool,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct PermissionTraceStep {
            outcome: PermissionStepOutcome,
            snapshot: PermissionRuntimeSnapshot,
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct PermissionModelState {
            snapshot: PermissionRuntimeSnapshot,
        }

        impl Default for PermissionModelState {
            fn default() -> Self {
                Self {
                    snapshot: PermissionRuntimeSnapshot {
                        active: true,
                        committed_value: 7,
                        staged_value: None,
                        read: false,
                        write: false,
                    },
                }
            }
        }

        impl PermissionModelState {
            fn apply(&mut self, op: PermissionModelOp) -> PermissionStepOutcome {
                match op {
                    PermissionModelOp::Begin => {
                        if self.snapshot.active {
                            return PermissionStepOutcome::Unit { ok: false };
                        }
                        self.snapshot.active = true;
                        self.snapshot.staged_value = None;
                        self.snapshot.read = false;
                        self.snapshot.write = false;
                        PermissionStepOutcome::Unit { ok: true }
                    }
                    PermissionModelOp::GrantRead => {
                        if !self.snapshot.active {
                            return PermissionStepOutcome::Bool {
                                ok: false,
                                value: None,
                            };
                        }
                        let granted = !self.snapshot.read;
                        self.snapshot.read = true;
                        PermissionStepOutcome::Bool {
                            ok: true,
                            value: Some(granted),
                        }
                    }
                    PermissionModelOp::GrantWrite => {
                        if !self.snapshot.active {
                            return PermissionStepOutcome::Bool {
                                ok: false,
                                value: None,
                            };
                        }
                        let granted = !self.snapshot.write;
                        self.snapshot.read = true;
                        self.snapshot.write = true;
                        PermissionStepOutcome::Bool {
                            ok: true,
                            value: Some(granted),
                        }
                    }
                    PermissionModelOp::Read => {
                        if !self.snapshot.active || !self.snapshot.read {
                            return PermissionStepOutcome::Read {
                                ok: false,
                                value: None,
                            };
                        }
                        PermissionStepOutcome::Read {
                            ok: true,
                            value: Some(
                                self.snapshot
                                    .staged_value
                                    .unwrap_or(self.snapshot.committed_value),
                            ),
                        }
                    }
                    PermissionModelOp::Write(value) => {
                        if !self.snapshot.active || !self.snapshot.write {
                            return PermissionStepOutcome::Write { ok: false };
                        }
                        self.snapshot.staged_value = Some(value);
                        PermissionStepOutcome::Write { ok: true }
                    }
                    PermissionModelOp::Downgrade => {
                        if !self.snapshot.active {
                            return PermissionStepOutcome::Bool {
                                ok: false,
                                value: None,
                            };
                        }
                        let removed = self.snapshot.write;
                        self.snapshot.write = false;
                        PermissionStepOutcome::Bool {
                            ok: true,
                            value: Some(removed),
                        }
                    }
                    PermissionModelOp::Abort => {
                        if !self.snapshot.active {
                            return PermissionStepOutcome::Unit { ok: false };
                        }
                        self.snapshot.active = false;
                        self.snapshot.staged_value = None;
                        self.snapshot.read = false;
                        self.snapshot.write = false;
                        PermissionStepOutcome::Unit { ok: true }
                    }
                    PermissionModelOp::CommitRelease => {
                        if !self.snapshot.active {
                            return PermissionStepOutcome::Unit { ok: false };
                        }
                        if let Some(value) = self.snapshot.staged_value {
                            self.snapshot.committed_value = value;
                        }
                        self.snapshot.active = false;
                        self.snapshot.staged_value = None;
                        self.snapshot.read = false;
                        self.snapshot.write = false;
                        PermissionStepOutcome::Unit { ok: true }
                    }
                }
            }
        }

        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        struct GranulePermissionModelState {
            active: bool,
            read: bool,
            write: bool,
        }

        impl Default for GranulePermissionModelState {
            fn default() -> Self {
                Self {
                    active: true,
                    read: false,
                    write: false,
                }
            }
        }

        fn permission_model_object() -> Result<(ObjectTable, ObjectId)> {
            let mut objects = ObjectTable::default();
            let object =
                objects.allocate_persistent_struct_for_gc_ref(0x611, vec![ObjectValue::I32(7)])?;
            Ok((objects, object))
        }

        fn payload_i32(payload: &ObjectPayload) -> Result<i32> {
            let ObjectPayload::Struct(fields) = payload else {
                bail!("permission model expected struct payload");
            };
            let field = fields
                .first()
                .context("permission model expected one struct field")?;
            let ObjectValue::I32(value) = field else {
                bail!("permission model expected i32 field");
            };
            Ok(*value)
        }

        fn downgrade_granule_write_for_test(
            state: &mut TransactionState,
            granule: GranuleId,
        ) -> Result<bool> {
            let Some(transaction) = state.active else {
                bail!("transaction operation requires an active transaction");
            };
            let removed = state.write_granules.remove(&granule);
            if removed {
                ensure!(
                    state.read_granules.contains(&granule),
                    "permission downgrade should preserve read permission for {granule:?}"
                );
                ensure!(
                    state.locks.owners.remove(&granule) == Some(transaction),
                    "permission downgrade expected tx {transaction:?} to own {granule:?}"
                );
            }
            Ok(removed)
        }

        fn apply_permission_runtime_op(
            state: &mut TransactionState,
            objects: &mut ObjectTable,
            object: ObjectId,
            op: PermissionModelOp,
        ) -> Result<PermissionStepOutcome> {
            match op {
                PermissionModelOp::GrantRead => match state.acquire_object_read(objects, object) {
                    Ok(granted) => Ok(PermissionStepOutcome::Bool {
                        ok: true,
                        value: Some(granted),
                    }),
                    Err(_) => Ok(PermissionStepOutcome::Bool {
                        ok: false,
                        value: None,
                    }),
                },
                PermissionModelOp::Begin => match state.begin() {
                    Ok(_) => Ok(PermissionStepOutcome::Unit { ok: true }),
                    Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
                },
                PermissionModelOp::GrantWrite => {
                    match state.acquire_object_write(objects, object) {
                        Ok(granted) => Ok(PermissionStepOutcome::Bool {
                            ok: true,
                            value: Some(granted),
                        }),
                        Err(_) => Ok(PermissionStepOutcome::Bool {
                            ok: false,
                            value: None,
                        }),
                    }
                }
                PermissionModelOp::Read => match state.read_struct_field(objects, object, 0) {
                    Ok(ObjectValue::I32(value)) => Ok(PermissionStepOutcome::Read {
                        ok: true,
                        value: Some(value),
                    }),
                    Ok(other) => bail!("permission model expected i32 field, got {other:?}"),
                    Err(_) => Ok(PermissionStepOutcome::Read {
                        ok: false,
                        value: None,
                    }),
                },
                PermissionModelOp::Write(value) => {
                    match state.stage_struct_field(objects, object, 0, ObjectValue::I32(value)) {
                        Ok(()) => Ok(PermissionStepOutcome::Write { ok: true }),
                        Err(_) => Ok(PermissionStepOutcome::Write { ok: false }),
                    }
                }
                PermissionModelOp::Downgrade => {
                    let granule = objects.granule_id(object)?;
                    match downgrade_granule_write_for_test(state, granule) {
                        Ok(removed) => Ok(PermissionStepOutcome::Bool {
                            ok: true,
                            value: Some(removed),
                        }),
                        Err(_) => Ok(PermissionStepOutcome::Bool {
                            ok: false,
                            value: None,
                        }),
                    }
                }
                PermissionModelOp::Abort => match state.abort() {
                    Ok(()) => Ok(PermissionStepOutcome::Unit { ok: true }),
                    Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
                },
                PermissionModelOp::CommitRelease => match state
                    .commit_object_payloads(objects)
                    .and_then(|_| state.complete_commit())
                {
                    Ok(()) => Ok(PermissionStepOutcome::Unit { ok: true }),
                    Err(_) => Ok(PermissionStepOutcome::Unit { ok: false }),
                },
            }
        }

        fn permission_runtime_snapshot(
            state: &TransactionState,
            objects: &ObjectTable,
            object: ObjectId,
        ) -> Result<PermissionRuntimeSnapshot> {
            let granule = objects.granule_id(object)?;
            let committed_value = payload_i32(&objects.payload(object)?)?;
            let staged_value = state
                .staged_objects
                .get(&object)
                .map(payload_i32)
                .transpose()?;
            Ok(PermissionRuntimeSnapshot {
                active: state.active.is_some(),
                committed_value,
                staged_value,
                read: state.read_granules.contains(&granule),
                write: state.write_granules.contains(&granule),
            })
        }

        fn run_permission_semantic_schedule(
            schedule: &[PermissionModelOp],
        ) -> Result<Vec<PermissionTraceStep>> {
            let outcome = (|| -> Result<Vec<PermissionTraceStep>> {
                let (mut objects, object) = permission_model_object()?;
                let mut state = TransactionState::new_for_test(TransactionId::from_raw(91));
                let mut model = PermissionModelState::default();
                let mut prefix = Vec::with_capacity(schedule.len());
                let mut trace = Vec::with_capacity(schedule.len());

                let initial = permission_runtime_snapshot(&state, &objects, object)?;
                ensure!(
                    initial == model.snapshot,
                    "initial permission snapshot diverged\nexpected: {:?}\nactual:   {:?}",
                    model.snapshot,
                    initial
                );

                for &op in schedule {
                    prefix.push(op);
                    let expected_outcome = model.apply(op);
                    let actual_outcome =
                        apply_permission_runtime_op(&mut state, &mut objects, object, op)?;
                    ensure!(
                        actual_outcome == expected_outcome,
                        "permission prefix {prefix:?} produced the wrong outcome\nexpected: {:?}\nactual:   {:?}",
                        expected_outcome,
                        actual_outcome
                    );
                    let actual_snapshot = permission_runtime_snapshot(&state, &objects, object)?;
                    ensure!(
                        actual_snapshot == model.snapshot,
                        "permission prefix {prefix:?} diverged\nexpected: {:?}\nactual:   {:?}",
                        model.snapshot,
                        actual_snapshot
                    );
                    trace.push(PermissionTraceStep {
                        outcome: actual_outcome,
                        snapshot: actual_snapshot,
                    });
                }

                Ok(trace)
            })();
            clear_current_thread_transaction_for_test();
            outcome
        }

        fn granule_permission_snapshot(
            state: &TransactionState,
            granule: GranuleId,
        ) -> GranulePermissionModelState {
            GranulePermissionModelState {
                active: state.active.is_some(),
                read: state.read_granules.contains(&granule),
                write: state.write_granules.contains(&granule),
            }
        }

        fn assert_generic_granule_reacquires_after_abort(granule: GranuleId) {
            let mut state = TransactionState::new_for_test(TransactionId::from_raw(101));
            assert!(state.acquire_granule_write(granule, 0).unwrap());
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: true,
                    read: true,
                    write: true,
                }
            );
            state.abort().unwrap();
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: false,
                    read: false,
                    write: false,
                }
            );
            state.begin().unwrap();
            assert!(state.acquire_granule_write(granule, 1).unwrap());
            state.abort().unwrap();
            clear_current_thread_transaction_for_test();
        }

        fn assert_generic_granule_reacquires_after_commit(granule: GranuleId) {
            let mut state = TransactionState::new_for_test(TransactionId::from_raw(102));
            assert!(state.acquire_granule_read(granule, 0).unwrap());
            assert!(state.acquire_granule_write(granule, 0).unwrap());
            state.complete_commit().unwrap();
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: false,
                    read: false,
                    write: false,
                }
            );
            state.begin().unwrap();
            assert!(state.acquire_granule_read(granule, 1).unwrap());
            assert!(state.acquire_granule_write(granule, 1).unwrap());
            state.abort().unwrap();
            clear_current_thread_transaction_for_test();
        }

        #[test]
        fn model_permissions_write_without_permission_fails_without_mutating_state() {
            let trace = run_permission_semantic_schedule(&[PermissionModelOp::Write(9)]).unwrap();

            assert_eq!(trace.len(), 1);
            assert_eq!(trace[0].snapshot, PermissionModelState::default().snapshot);
            assert_eq!(trace[0].outcome, PermissionStepOutcome::Write { ok: false });
        }

        #[test]
        fn model_permissions_read_permission_allows_read_without_write() {
            let trace = run_permission_semantic_schedule(&[
                PermissionModelOp::GrantRead,
                PermissionModelOp::Read,
                PermissionModelOp::Write(9),
            ])
            .unwrap();

            assert_eq!(trace[2].outcome, PermissionStepOutcome::Write { ok: false });
            assert_eq!(
                trace[0].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[1].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(7),
                }
            );
            assert_eq!(
                trace[2].snapshot,
                PermissionRuntimeSnapshot {
                    active: true,
                    committed_value: 7,
                    staged_value: None,
                    read: true,
                    write: false,
                }
            );
        }

        #[test]
        fn model_permissions_write_permission_grants_staged_mutation_and_reads_staged_value() {
            let trace = run_permission_semantic_schedule(&[
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(9),
                PermissionModelOp::Read,
            ])
            .unwrap();

            assert_eq!(
                trace[0].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(trace[1].outcome, PermissionStepOutcome::Write { ok: true });
            assert_eq!(
                trace[2].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(9),
                }
            );
            assert_eq!(
                trace[2].snapshot,
                PermissionRuntimeSnapshot {
                    active: true,
                    committed_value: 7,
                    staged_value: Some(9),
                    read: true,
                    write: true,
                }
            );
        }

        #[test]
        fn model_permissions_downgrade_rejects_future_writes_until_reacquired() {
            let trace = run_permission_semantic_schedule(&[
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(9),
                PermissionModelOp::Downgrade,
                PermissionModelOp::Read,
                PermissionModelOp::Write(10),
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(10),
                PermissionModelOp::Read,
            ])
            .unwrap();

            assert_eq!(trace[4].outcome, PermissionStepOutcome::Write { ok: false });
            assert_eq!(
                trace[2].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[3].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(9),
                }
            );
            assert_eq!(
                trace[5].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[7].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(10),
                }
            );
        }

        #[test]
        fn model_permissions_abort_discards_state() {
            let trace = run_permission_semantic_schedule(&[
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(9),
                PermissionModelOp::Abort,
                PermissionModelOp::Read,
                PermissionModelOp::Begin,
                PermissionModelOp::GrantRead,
                PermissionModelOp::Read,
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(10),
                PermissionModelOp::Read,
            ])
            .unwrap();

            assert_eq!(
                trace[3].outcome,
                PermissionStepOutcome::Read {
                    ok: false,
                    value: None,
                }
            );
            assert_eq!(trace[2].outcome, PermissionStepOutcome::Unit { ok: true });
            assert_eq!(trace[4].outcome, PermissionStepOutcome::Unit { ok: true });
            assert_eq!(
                trace[5].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[6].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(7),
                }
            );
            assert_eq!(
                trace[7].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[9].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(10),
                }
            );
            assert_eq!(
                trace[3].snapshot,
                PermissionRuntimeSnapshot {
                    active: false,
                    committed_value: 7,
                    staged_value: None,
                    read: false,
                    write: false,
                }
            );
        }

        #[test]
        fn model_permissions_commit_release_clears_permissions_and_commits_value() {
            let trace = run_permission_semantic_schedule(&[
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(9),
                PermissionModelOp::CommitRelease,
                PermissionModelOp::Read,
                PermissionModelOp::Begin,
                PermissionModelOp::GrantRead,
                PermissionModelOp::Read,
                PermissionModelOp::GrantWrite,
                PermissionModelOp::Write(10),
                PermissionModelOp::Read,
            ])
            .unwrap();

            assert_eq!(
                trace[3].outcome,
                PermissionStepOutcome::Read {
                    ok: false,
                    value: None,
                }
            );
            assert_eq!(trace[2].outcome, PermissionStepOutcome::Unit { ok: true });
            assert_eq!(trace[4].outcome, PermissionStepOutcome::Unit { ok: true });
            assert_eq!(
                trace[5].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[6].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(9),
                }
            );
            assert_eq!(
                trace[7].outcome,
                PermissionStepOutcome::Bool {
                    ok: true,
                    value: Some(true),
                }
            );
            assert_eq!(
                trace[9].outcome,
                PermissionStepOutcome::Read {
                    ok: true,
                    value: Some(10),
                }
            );
            assert_eq!(
                trace[3].snapshot,
                PermissionRuntimeSnapshot {
                    active: false,
                    committed_value: 9,
                    staged_value: None,
                    read: false,
                    write: false,
                }
            );
        }

        #[test]
        fn model_permissions_generic_granule_state_is_not_object_specific() {
            let mut state = TransactionState::new_for_test(TransactionId::from_raw(92));
            let granule = GranuleId::TGlobal {
                instance: Some(4),
                global_index: 1,
            };

            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState::default()
            );

            assert!(state.acquire_granule_read(granule, 0).unwrap());
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: true,
                    read: true,
                    write: false,
                }
            );

            assert!(state.acquire_granule_write(granule, 0).unwrap());
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: true,
                    read: true,
                    write: true,
                }
            );

            state.complete_commit().unwrap();
            assert_eq!(
                granule_permission_snapshot(&state, granule),
                GranulePermissionModelState {
                    active: false,
                    read: false,
                    write: false,
                }
            );
            clear_current_thread_transaction_for_test();
        }

        #[test]
        fn model_permissions_ttable_granule_reacquires_after_abort() {
            assert_generic_granule_reacquires_after_abort(GranuleId::TTable {
                instance: Some(3),
                table_index: 2,
                granule_index: 1,
            });
        }

        #[test]
        fn model_permissions_ttable_size_reacquires_after_commit() {
            assert_generic_granule_reacquires_after_commit(GranuleId::TTableSize {
                instance: Some(3),
                table_index: 2,
            });
        }

        #[test]
        fn model_permissions_tglobal_reacquires_after_commit() {
            assert_generic_granule_reacquires_after_commit(GranuleId::TGlobal {
                instance: Some(3),
                global_index: 4,
            });
        }

        fn permission_model_op_strategy() -> impl Strategy<Value = PermissionModelOp> {
            prop_oneof![
                Just(PermissionModelOp::Begin),
                Just(PermissionModelOp::GrantRead),
                Just(PermissionModelOp::GrantWrite),
                Just(PermissionModelOp::Read),
                (0i32..=15).prop_map(PermissionModelOp::Write),
                Just(PermissionModelOp::Downgrade),
                Just(PermissionModelOp::Abort),
                Just(PermissionModelOp::CommitRelease),
            ]
        }

        #[test]
        fn model_permissions_generated_short_sequences_match_reference_world() {
            let mut runner = TestRunner::new(Config {
                cases: 64,
                failure_persistence: None,
                max_shrink_iters: 256,
                rng_seed: RngSeed::Fixed(0x5eed_0006),
                ..Config::default()
            });

            runner
                .run(
                    &prop::collection::vec(permission_model_op_strategy(), 1..=10usize),
                    |schedule| {
                        run_permission_semantic_schedule(&schedule)
                            .map_err(|error| TestCaseError::fail(error.to_string()))?;
                        Ok(())
                    },
                )
                .unwrap();
        }
    }

    #[test]
    fn transaction_object_tstruct_field_helpers_read_staged_payload_before_committed_record() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::I64(2)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();

        assert_eq!(
            state.read_struct_field(&objects, object, 0).unwrap(),
            ObjectValue::I32(1)
        );
        state
            .stage_struct_field(&objects, object, 0, ObjectValue::I32(9))
            .unwrap();

        assert_eq!(objects.version(object).unwrap(), 1);
        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::I64(2)])
        );
        assert_eq!(
            state.read_struct_field(&objects, object, 0).unwrap(),
            ObjectValue::I32(9)
        );
        assert!(state.owns_struct_write(object));

        assert!(state.commit_object_payloads(&mut objects).unwrap());
        state.complete_commit().unwrap();

        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(9), ObjectValue::I64(2)])
        );
        assert!(!state.owns_struct_write(object));
    }

    #[test]
    fn transaction_object_tstruct_field_abort_discards_staged_payload() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();
        state
            .stage_struct_field(&objects, object, 1, ObjectValue::I32(7))
            .unwrap();
        assert_eq!(
            state.read_struct_field(&objects, object, 1).unwrap(),
            ObjectValue::I32(7)
        );

        state.abort().unwrap();

        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::I32(2)])
        );
        assert!(!state.owns_struct_write(object));
    }

    #[test]
    fn transaction_object_abort_frees_objects_allocated_by_active_transaction() {
        let mut objects = ObjectTable::default();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
        state.record_allocated_object(object).unwrap();
        assert_eq!(objects.live_count(), 1);

        state.abort_allocated_objects(&mut objects).unwrap();

        assert!(state.active_transaction().is_none());
        assert_eq!(objects.live_count(), 0);
        assert!(objects.kind(object).is_err());

        let reused = objects.allocate_array(vec![ObjectValue::I32(2)]).unwrap();
        assert_eq!(reused, object);
    }

    #[test]
    fn transaction_object_commit_keeps_objects_allocated_by_active_transaction() {
        let mut objects = ObjectTable::default();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
        state.record_allocated_object(object).unwrap();
        state.complete_commit().unwrap();

        assert_eq!(objects.live_count(), 1);
        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1)])
        );

        state.begin().unwrap();
        state.abort_allocated_objects(&mut objects).unwrap();

        assert_eq!(objects.live_count(), 1);
        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1)])
        );
    }

    #[test]
    fn transaction_object_tarray_helpers_stage_whole_object_and_commit_ranges() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_array(vec![
                ObjectValue::I32(0),
                ObjectValue::I32(1),
                ObjectValue::I32(2),
                ObjectValue::I32(3),
                ObjectValue::I32(4),
            ])
            .unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();

        assert_eq!(state.read_array_len(&objects, object).unwrap(), 5);
        assert_eq!(
            state.read_array_element(&objects, object, 2).unwrap(),
            ObjectValue::I32(2)
        );

        state
            .stage_array_element(&objects, object, 2, ObjectValue::I32(20))
            .unwrap();
        state
            .fill_array_range(&objects, object, 3, 2, ObjectValue::I32(9))
            .unwrap();
        state
            .copy_array_range(&objects, object, 0, object, 2, 3)
            .unwrap();
        state
            .write_array_range(
                &objects,
                object,
                1,
                vec![ObjectValue::I32(30), ObjectValue::I32(31)],
            )
            .unwrap();

        assert_eq!(
            state.read_object_payload(&objects, object).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::I32(20),
                ObjectValue::I32(30),
                ObjectValue::I32(31),
                ObjectValue::I32(9),
                ObjectValue::I32(9),
            ])
        );
        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::I32(0),
                ObjectValue::I32(1),
                ObjectValue::I32(2),
                ObjectValue::I32(3),
                ObjectValue::I32(4),
            ])
        );
        assert!(state.owns_array_write(object));

        assert!(state.commit_object_payloads(&mut objects).unwrap());
        state.complete_commit().unwrap();

        assert_eq!(
            objects.payload(object).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::I32(20),
                ObjectValue::I32(30),
                ObjectValue::I32(31),
                ObjectValue::I32(9),
                ObjectValue::I32(9),
            ])
        );
    }

    #[test]
    fn transaction_object_tarray_range_helpers_validate_bounds() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_array(vec![ObjectValue::I32(0)]).unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_write(&objects, object).unwrap();

        let error = state.read_array_element(&objects, object, 1).unwrap_err();
        assert!(error.to_string().contains("out of bounds array access"));

        let error = state
            .fill_array_range(&objects, object, 1, 1, ObjectValue::I32(3))
            .unwrap_err();
        assert!(error.to_string().contains("out of bounds array access"));
    }

    #[test]
    fn transaction_object_ti31_is_immediate_and_does_not_allocate_object_record() {
        let objects = ObjectTable::default();
        let mut state = TransactionState::default();

        state.begin().unwrap();

        let positive = state.create_i31(0x4000_0001).unwrap();
        assert_eq!(positive.get_u(), 0x4000_0001);
        assert_eq!(positive.get_s(), -0x3fff_ffff);

        let negative = state.create_i31(-1).unwrap();
        assert_eq!(negative.get_u(), 0x7fff_ffff);
        assert_eq!(negative.get_s(), -1);

        assert_eq!(objects.live_count(), 0);
        assert_eq!(objects.slot_count(), 0);
    }

    #[test]
    fn transaction_object_unsupported_textern_promotion_aborts_transaction() {
        let mut state = TransactionState::default();

        state.begin().unwrap();

        let error = state
            .promote_extern_ref_for_persistence(0x1234)
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("persistent extern object promotion is not supported")
        );
        assert!(state.active_transaction().is_none());
    }

    #[test]
    fn object_payload_commit_validates_object_read_versions() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_read(&objects, object).unwrap();
        state.read_object_payload(&objects, object).unwrap();
        objects
            .update_payload(object, ObjectPayload::Struct(vec![ObjectValue::I32(2)]))
            .unwrap();

        let error = state.commit_object_payloads(&mut objects).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("optimistic read version changed")
        );
    }

    #[test]
    fn transaction_object_read_validation_uses_object_table_versions() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();
        let mut state = TransactionState::default();

        state.begin().unwrap();
        state.acquire_object_read(&objects, object).unwrap();

        state.validate_active_object_reads(&objects).unwrap();

        objects
            .update_payload(object, ObjectPayload::Struct(vec![ObjectValue::I32(2)]))
            .unwrap();

        let error = state.validate_active_object_reads(&objects).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("optimistic read version changed")
        );
    }

    #[test]
    fn transaction_object_header_records_object_metadata() {
        let mut heap = object_heap::ObjectHeap::default();
        let object_id = ObjectId { object_index: 17 };
        let payload = ObjectPayload::Struct(vec![
            ObjectValue::I32(11),
            ObjectValue::Ref(Some(ObjectId { object_index: 3 })),
        ]);

        let handle = heap
            .allocate_record(object_id, 1, ObjectKind::Struct, 0x23, 41, &payload)
            .unwrap();
        let header = heap.header(handle).unwrap();

        assert_eq!(header.object_id, object_id.object_index);
        assert_eq!(header.version, 1);
        assert_eq!(header.kind, ObjectKind::Struct as u16);
        assert_eq!(header.flags, 0x23);
        assert_eq!(header.type_index, 41);
        assert_eq!(header.record_len, heap.record_len(handle).unwrap());
    }

    #[test]
    fn transaction_object_array_header_records_length() {
        let mut heap = object_heap::ObjectHeap::default();
        let object_id = ObjectId { object_index: 9 };
        let payload = ObjectPayload::Array(vec![
            ObjectValue::I64(1),
            ObjectValue::Ref(Some(ObjectId { object_index: 4 })),
            ObjectValue::Ref(None),
        ]);

        let handle = heap
            .allocate_record(object_id, 2, ObjectKind::Array, 0, 7, &payload)
            .unwrap();
        let header = heap.array_header(handle).unwrap();

        assert_eq!(header.base.object_id, object_id.object_index);
        assert_eq!(header.base.version, 2);
        assert_eq!(header.base.kind, ObjectKind::Array as u16);
        assert_eq!(header.base.type_index, 7);
        assert_eq!(header.length, 3);
        assert_eq!(header.base.record_len, heap.record_len(handle).unwrap());
    }

    #[test]
    fn transaction_object_heap_writes_records_to_block_region() {
        let mut heap = object_heap::ObjectHeap::default();
        let payload = ObjectPayload::Struct(vec![ObjectValue::I32(0x1122_3344)]);

        let handle = heap
            .allocate_record(
                ObjectId { object_index: 17 },
                3,
                ObjectKind::Struct,
                0x23,
                41,
                &payload,
            )
            .unwrap();

        let bytes = heap.record_bytes_for_test(handle).unwrap();
        let payload_offset = core::mem::size_of::<object_heap::TxObjectHeader>();
        assert_eq!(heap.record_offset_for_test(handle).unwrap(), 0);
        assert_eq!(
            u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            heap.record_len(handle).unwrap()
        );
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 17);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 3);
        assert_eq!(
            u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
            ObjectKind::Struct as u16
        );
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 0x23);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 41);
        assert_eq!(
            ObjectValueAbi::from_parts(
                u32::from_le_bytes(
                    bytes[payload_offset..payload_offset + 4]
                        .try_into()
                        .unwrap()
                ),
                u64::from_le_bytes(
                    bytes[payload_offset + 4..payload_offset + 12]
                        .try_into()
                        .unwrap()
                ),
                u64::from_le_bytes(
                    bytes[payload_offset + 12..payload_offset + 20]
                        .try_into()
                        .unwrap()
                ),
            )
            .unwrap()
            .to_object_value()
            .unwrap(),
            ObjectValue::I32(0x1122_3344)
        );
    }

    #[test]
    fn transaction_object_heap_marks_allocated_lines() {
        let mut heap = object_heap::ObjectHeap::default();
        let handle = heap
            .allocate_record(
                ObjectId { object_index: 3 },
                4,
                ObjectKind::Struct,
                0,
                0,
                &ObjectPayload::Struct(vec![ObjectValue::I64(9)]),
            )
            .unwrap();

        assert_eq!(heap.block_region_block_size_for_test(), 512 * 1024);
        assert_eq!(heap.immix_line_size_for_test(), 256);
        assert!(heap.line_mark_for_record_for_test(handle).unwrap());
    }

    #[test]
    fn transaction_object_id_stays_stable_when_record_handle_changes() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
            .unwrap();

        let first_handle = objects.current_record_handle_for_test(object).unwrap();
        let first_version = objects.version(object).unwrap();

        objects
            .update_payload(
                object,
                ObjectPayload::Struct(vec![
                    ObjectValue::I32(2),
                    ObjectValue::Ref(Some(ObjectId { object_index: 99 })),
                ]),
            )
            .unwrap();

        let second_handle = objects.current_record_handle_for_test(object).unwrap();
        let second_version = objects.version(object).unwrap();

        assert_eq!(object, ObjectId { object_index: 0 });
        assert_ne!(first_handle, second_handle);
        assert!(second_version > first_version);
    }

    #[test]
    fn transaction_object_records_serialize_version() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
            .unwrap();

        let first_handle = objects.current_record_handle_for_test(object).unwrap();
        let first_bytes = objects.heap.record_bytes_for_test(first_handle).unwrap();
        let first_version = u32::from_le_bytes(first_bytes[16..20].try_into().unwrap());

        objects
            .update_payload(
                object,
                ObjectPayload::Struct(vec![
                    ObjectValue::I32(2),
                    ObjectValue::Ref(Some(ObjectId { object_index: 99 })),
                ]),
            )
            .unwrap();

        let second_handle = objects.current_record_handle_for_test(object).unwrap();
        let second_bytes = objects.heap.record_bytes_for_test(second_handle).unwrap();
        let second_version = u32::from_le_bytes(second_bytes[16..20].try_into().unwrap());

        assert_eq!(first_version, 1);
        assert_eq!(second_version, 2);
    }

    #[test]
    fn commit_publishes_struct_object_update() {
        let mut objects = ObjectTable::default();
        let object = objects.allocate_struct(vec![ObjectValue::I32(1)]).unwrap();

        let pub_ = objects.pending_publication_for_test(object).unwrap();
        let (domain, object_id) =
            crate::runtime::vm::unpack_object_granule_id(pub_.logical_id).unwrap();
        assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TStruct);
        assert_eq!(object_id, object.object_index);
        assert_eq!(pub_.version, 1);
    }

    #[test]
    fn commit_publishes_array_object_update() {
        let mut objects = ObjectTable::default();
        let object = objects
            .allocate_array(vec![ObjectValue::Ref(None), ObjectValue::I64(9)])
            .unwrap();

        let pub_ = objects.pending_publication_for_test(object).unwrap();
        let (domain, object_id) =
            crate::runtime::vm::unpack_object_granule_id(pub_.logical_id).unwrap();
        assert_eq!(domain, crate::runtime::vm::PackedGranuleDomain::TArray);
        assert_eq!(object_id, object.object_index);
        assert_eq!(pub_.version, 1);
    }

    #[test]
    fn object_table_rebuilds_latest_slots_from_heap_publication_metadata() {
        let mut objects = ObjectTable::default();
        let first = objects
            .allocate_struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
            .unwrap();
        let second = objects
            .allocate_array(vec![ObjectValue::Ref(Some(first)), ObjectValue::I64(7)])
            .unwrap();

        objects
            .update_payload(
                first,
                ObjectPayload::Struct(vec![ObjectValue::I32(2), ObjectValue::Ref(Some(second))]),
            )
            .unwrap();

        let first_latest_handle = objects.current_record_handle_for_test(first).unwrap();
        let second_latest_handle = objects.current_record_handle_for_test(second).unwrap();
        let first_latest_payload = objects.payload(first).unwrap();
        let second_latest_payload = objects.payload(second).unwrap();

        objects.slots.clear();
        objects.free_list.clear();
        objects.gc_ref_to_object.clear();
        objects.object_to_gc_ref.clear();
        objects.func_ref_to_object.clear();
        objects.object_to_func_ref.clear();
        objects.next_version = 0;
        objects.live_count = 0;

        objects.rebuild_volatile_index_from_heap().unwrap();

        assert_eq!(
            objects.current_record_handle_for_test(first).unwrap(),
            first_latest_handle
        );
        assert_eq!(
            objects.current_record_handle_for_test(second).unwrap(),
            second_latest_handle
        );
        assert_eq!(objects.payload(first).unwrap(), first_latest_payload);
        assert_eq!(objects.payload(second).unwrap(), second_latest_payload);
        assert_eq!(objects.live_count(), 2);
    }

    #[test]
    fn object_index_rebuilds_from_recovered_object_winners() {
        let region = sample_region_with_two_object_winners();
        let recovered = crate::runtime::vm::recover_region_for_test(&region).unwrap();
        let winners = recovered.committed_object_winners().unwrap();
        let mut objects = ObjectTable::default();

        objects.rebuild_from_recovery_for_test(&winners).unwrap();

        assert_eq!(objects.live_count(), 2);
        assert_eq!(
            objects.kind(ObjectId { object_index: 41 }).unwrap(),
            ObjectKind::Struct
        );
        assert_eq!(
            objects.payload(ObjectId { object_index: 41 }).unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)])
        );
        assert_eq!(
            objects.kind(ObjectId { object_index: 42 }).unwrap(),
            ObjectKind::Array
        );
        assert_eq!(
            objects.payload(ObjectId { object_index: 42 }).unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
                ObjectValue::I64(9),
            ])
        );
    }

    #[test]
    fn transaction_object_scanner_returns_embedded_refs_from_struct_and_array_payloads() {
        let mut struct_heap = object_heap::ObjectHeap::default();
        let struct_handle = struct_heap
            .allocate_record(
                ObjectId { object_index: 1 },
                5,
                ObjectKind::Struct,
                0,
                3,
                &ObjectPayload::Struct(vec![
                    ObjectValue::I32(7),
                    ObjectValue::Ref(Some(ObjectId { object_index: 11 })),
                    ObjectValue::Ref(None),
                    ObjectValue::V128([0; 16]),
                    ObjectValue::Ref(Some(ObjectId { object_index: 12 })),
                ]),
            )
            .unwrap();

        assert_eq!(
            struct_heap.trace_object_ids(struct_handle).unwrap(),
            vec![ObjectId { object_index: 11 }, ObjectId { object_index: 12 }]
        );

        let mut array_heap = object_heap::ObjectHeap::default();
        let array_handle = array_heap
            .allocate_record(
                ObjectId { object_index: 2 },
                6,
                ObjectKind::Array,
                0,
                4,
                &ObjectPayload::Array(vec![
                    ObjectValue::Ref(None),
                    ObjectValue::Ref(Some(ObjectId { object_index: 21 })),
                    ObjectValue::I64(4),
                    ObjectValue::Ref(Some(ObjectId { object_index: 22 })),
                ]),
            )
            .unwrap();

        assert_eq!(
            array_heap.trace_object_ids(array_handle).unwrap(),
            vec![ObjectId { object_index: 21 }, ObjectId { object_index: 22 }]
        );
    }

    fn sample_region_with_two_object_winners()
    -> crate::runtime::vm::block_region::VMemoryBlockRegion {
        let mut region =
            crate::runtime::vm::block_region::VMemoryBlockRegion::new_for_test(32).unwrap();
        let stream1 = region.alloc_stream(1).unwrap();
        let stream2 = region.alloc_stream(2).unwrap();

        let first = encoded_object_publication_for_recovery_test(
            41,
            1,
            7,
            ObjectPayload::Struct(vec![ObjectValue::I32(1), ObjectValue::Ref(None)]),
        );
        let second = encoded_object_publication_for_recovery_test(
            42,
            1,
            9,
            ObjectPayload::Array(vec![
                ObjectValue::Ref(Some(ObjectId { object_index: 41 })),
                ObjectValue::I64(9),
            ]),
        );

        append_committed_object_winner(&mut region, 1, stream1, 0, &first);
        append_committed_object_winner(&mut region, 2, stream2, 0, &second);

        region
    }

    fn encoded_object_publication_for_recovery_test(
        object_id: u64,
        version: u32,
        type_index: u32,
        payload: ObjectPayload,
    ) -> persist::PendingPublication {
        persist::PendingPublication::persistent_object(
            match payload.kind() {
                ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
                ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
                _ => unreachable!(),
            },
            object_id,
            version,
            type_index,
            encode_object_record_for_test(object_id, version, type_index, &payload).unwrap(),
        )
        .unwrap()
    }

    fn append_committed_object_winner(
        region: &mut crate::runtime::vm::block_region::VMemoryBlockRegion,
        stream_id: u32,
        stream: crate::runtime::vm::block_region::StreamCursor,
        block_seq: u32,
        publication: &persist::PendingPublication,
    ) {
        let record = TMemory::encode_publication_data_record(
            publication.logical_id,
            publication.version,
            publication.kind,
            publication.type_info,
            &publication.payload,
        )
        .unwrap();
        let location = region.append_data_record(stream, &record).unwrap();
        let log_block = region.alloc_log_block(stream_id, block_seq).unwrap();
        let entry = TMemory::publication_log_entry(
            publication.logical_id,
            publication.version,
            stream_id << 1,
            location.data_block,
            location.data_offset,
            true,
        );
        write_log_entry_for_recovery_test(region, log_block, entry);
    }

    fn write_log_entry_for_recovery_test(
        region: &mut crate::runtime::vm::block_region::VMemoryBlockRegion,
        start_block: u32,
        entry: crate::runtime::vm::TxLogEntry,
    ) {
        let mut header = region.log_block_header(start_block).unwrap();
        header.entry_count = 1;
        let block_offset =
            usize::try_from(start_block).unwrap() * crate::runtime::vm::block_region::BLOCK_SIZE;
        region.write(block_offset, &header.as_bytes()).unwrap();
        let offset = block_offset + header.as_bytes().len();
        region
            .write(offset, &encode_tx_log_entry_for_recovery_test(entry))
            .unwrap();
    }

    fn encode_tx_log_entry_for_recovery_test(entry: crate::runtime::vm::TxLogEntry) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0..8].copy_from_slice(&entry.logical_id.to_le_bytes());
        bytes[8..12].copy_from_slice(&entry.version.to_le_bytes());
        bytes[12..16].copy_from_slice(&entry.tx_meta.to_le_bytes());
        bytes[16..20].copy_from_slice(&entry.data_block.to_le_bytes());
        bytes[20..24].copy_from_slice(&entry.data_offset.to_le_bytes());
        bytes[24..28].copy_from_slice(&entry.crc32.to_le_bytes());
        bytes[28..32].copy_from_slice(&entry.reserved.to_le_bytes());
        bytes
    }

    #[test]
    fn transaction_object_trace_descriptor_distinguishes_scalar_arrays() {
        let mut heap = object_heap::ObjectHeap::default();
        let handle = heap
            .allocate_record(
                ObjectId { object_index: 3 },
                7,
                ObjectKind::Array,
                0,
                8,
                &ObjectPayload::Array(vec![ObjectValue::I32(1), ObjectValue::I32(2)]),
            )
            .unwrap();

        assert_eq!(
            heap.trace_descriptor(handle).unwrap(),
            object_heap::TraceDescriptor::Array(object_heap::TraceArrayDescriptor {
                element_kind: object_heap::TraceValueKind::Scalar,
                length: 2,
            })
        );
    }

    #[test]
    fn object_table_reports_embedded_refs_from_current_record() {
        let mut objects = ObjectTable::default();
        let first = ObjectId { object_index: 8 };
        let second = ObjectId { object_index: 9 };
        let object = objects
            .allocate_struct(vec![
                ObjectValue::I32(1),
                ObjectValue::Ref(Some(first)),
                ObjectValue::Ref(None),
            ])
            .unwrap();

        assert_eq!(objects.trace_object_ids(object).unwrap(), vec![first]);

        objects
            .update_payload(
                object,
                ObjectPayload::Struct(vec![
                    ObjectValue::Ref(Some(second)),
                    ObjectValue::I64(3),
                    ObjectValue::Ref(Some(first)),
                ]),
            )
            .unwrap();

        assert_eq!(
            objects.trace_object_ids(object).unwrap(),
            vec![second, first]
        );
    }

    #[test]
    fn lock_based_allows_shared_reads_and_writer_upgrade() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 7,
        };

        locks.record_read_for_test(first, granule, 5).unwrap();
        locks.record_read_for_test(second, granule, 5).unwrap();
        locks.acquire_write_for_test(second, granule, 5).unwrap();
    }

    #[test]
    fn lock_based_writer_excludes_other_transactions() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 7,
        };

        locks.acquire_write_for_test(first, granule, 3).unwrap();

        let read_error = locks
            .record_read_result_for_test(second, granule, 3)
            .unwrap_err();
        assert_eq!(read_error, LockBasedConflictKindForTest::ReadOwnedByOther);

        let write_error = locks
            .acquire_write_result_for_test(second, granule, 3)
            .unwrap_err();
        assert_eq!(write_error, LockBasedConflictKindForTest::WriteOwnedByOther);
    }

    #[test]
    fn lock_based_conflicts_are_released_on_abort() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_write(first, 0, 0).unwrap();
        assert!(locks.acquire_memory_granule_read(second, 0, 0).is_err());

        locks.release_transaction(first);

        locks.acquire_memory_granule_read(second, 0, 0).unwrap();
    }

    #[test]
    fn lock_based_supports_upgrade_and_release() {
        let mut locks = LockBased::default();
        let first = TransactionId(1);
        let second = TransactionId(2);

        locks.acquire_memory_granule_read(first, 0, 7).unwrap();
        locks.acquire_memory_granule_write(first, 0, 7).unwrap();
        locks.release_transaction(first);

        locks.acquire_memory_granule_write(second, 0, 7).unwrap();
    }

    #[test]
    fn lock_based_write_conflict_aborts_current_transaction() {
        let mut locks = LockBased::default();
        let first = TransactionId::from_raw(1);
        let second = TransactionId::from_raw(2);
        let conflicted = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };
        let independent = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 1,
        };

        locks.acquire_write_for_test(first, conflicted, 9).unwrap();
        locks
            .acquire_write_for_test(second, independent, 3)
            .unwrap();

        let error = locks
            .acquire_write_result_for_test(second, conflicted, 9)
            .unwrap_err();
        assert_eq!(error, LockBasedConflictKindForTest::WriteOwnedByOther);
        assert_eq!(locks.owner_for_test(independent), Some(second));

        locks.abort_for_test(second);

        assert_eq!(locks.owner_for_test(conflicted), Some(first));
        assert_eq!(locks.owner_for_test(independent), None);
    }

    #[test]
    fn lock_based_validates_optimistic_reads_at_commit() {
        let mut locks = LockBased::default();
        let reader = TransactionId::from_raw(1);
        let writer = TransactionId::from_raw(2);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };

        locks.record_read_for_test(reader, granule, 7).unwrap();
        locks.acquire_write_for_test(writer, granule, 7).unwrap();
        locks.abort_for_test(writer);

        let error = locks
            .validate_read_result_for_test(reader, granule, 8)
            .unwrap_err();
        assert_eq!(error, LockBasedConflictKindForTest::ReadVersionMismatch);
    }

    #[test]
    fn lock_based_abort_releases_owned_granules() {
        let mut locks = LockBased::default();
        let transaction = TransactionId::from_raw(1);
        let granule = GranuleId::TMemory {
            instance: Some(1),
            memory_index: 0,
            granule_index: 0,
        };

        locks
            .acquire_write_for_test(transaction, granule, 4)
            .unwrap();
        assert_eq!(locks.owner_for_test(granule), Some(transaction));

        locks.abort_for_test(transaction);

        assert_eq!(locks.owner_for_test(granule), None);
    }

    #[test]
    fn fixture_executor_begins_and_fails_transaction() {
        let mut state = TransactionState::default();
        let operators = [
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 0,
                operator: wasmtime_environ::TransactionOperator::TTry,
                bytes_read: 2,
            },
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 2,
                operator: wasmtime_environ::TransactionOperator::TFail,
                bytes_read: 2,
            },
        ];

        execute_research_transaction_fixture(&mut state, &operators).unwrap();

        assert_eq!(state.active_transaction(), None);
        assert!(state.structured_failure_pending());
        state.clear_structured_failure();
        assert!(!state.structured_failure_pending());
    }

    #[test]
    fn fixture_executor_ttry_end_consumes_pending_failure() {
        let mut state = TransactionState::default();
        let operators = [
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 0,
                operator: wasmtime_environ::TransactionOperator::TTry,
                bytes_read: 2,
            },
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 2,
                operator: wasmtime_environ::TransactionOperator::TFail,
                bytes_read: 2,
            },
            wasmtime_environ::ResearchTransactionModuleOperator {
                function_index: 0,
                body_offset: 4,
                operator: wasmtime_environ::TransactionOperator::TTryEnd,
                bytes_read: 2,
            },
        ];

        execute_research_transaction_fixture(&mut state, &operators).unwrap();

        assert_eq!(state.active_transaction(), None);
        assert!(!state.structured_failure_pending());
    }

    #[test]
    fn fixture_executor_commits_open_transaction_at_end() {
        let mut state = TransactionState::default();
        let operators = [wasmtime_environ::ResearchTransactionModuleOperator {
            function_index: 0,
            body_offset: 0,
            operator: wasmtime_environ::TransactionOperator::TTry,
            bytes_read: 2,
        }];

        execute_research_transaction_fixture(&mut state, &operators).unwrap();

        assert_eq!(state.active_transaction(), None);
    }

    #[test]
    fn module_compilation_accepts_lifecycle_transaction_opcodes() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x0a, 0x08, 0x01, 0x06, 0x00, // code section/function body
            0xfa, 0x04, // ttry
            0xfa, 0x0f, // tfail
            0x0b, // end
        ];

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_data_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_rejects_transaction_load_on_ordinary_memory() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // ordinary memory section
            0x0a, 0x0a, 0x01, 0x08, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0xfa, 0x28, 0x02, 0x00, // i32.tload align=2 offset=0
            0x0b, // end
        ];

        let error = crate::Module::new(&engine, wasm).unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional memory operator requires tmemory"),
            "{error}"
        );
    }

    #[test]
    fn module_compilation_accepts_transaction_store_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x0c, 0x01, 0x0a, 0x00, // code section/function body
            0x41, 0x00, // i32.const 0
            0x41, 0x2a, // i32.const 42
            0xfa, 0x36, 0x02, 0x00, // i32.tstore align=2 offset=0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_global_get_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_global_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x23, 0x00, // tglobal.get 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_rejects_transaction_global_get_on_ordinary_global() {
        let engine = crate::Engine::default();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x00, 0x41, 0x00, 0x0b, // ordinary global section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x23, 0x00, // tglobal.get 0
            0x0b, // end
        ];

        let error = crate::Module::new(&engine, wasm).unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional global operator requires tglobal"),
            "{error}"
        );
    }

    #[test]
    fn module_compilation_accepts_transaction_global_set_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_global_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x06, 0x06, 0x01, 0x7f, 0x01, 0x41, 0x00, 0x0b, // global section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x2a, // i32.const 42
            0xfa, 0x24, 0x00, // tglobal.set 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_size_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x07, 0x01, 0x05, 0x00, // code section/function body
            0xfa, 0x3f, 0x00, // tmemory.size 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_memory_grow_helper_lowering() {
        let engine = crate::Engine::default();
        let wasm = with_transaction_memory_metadata(&[
            0x00, 0x61, 0x73, 0x6d, // magic
            0x01, 0x00, 0x00, 0x00, // version
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7f, // type section
            0x03, 0x02, 0x01, 0x00, // function section
            0x05, 0x03, 0x01, 0x00, 0x01, // memory section
            0x0a, 0x09, 0x01, 0x07, 0x00, // code section/function body
            0x41, 0x01, // i32.const 1
            0xfa, 0x40, 0x00, // tmemory.grow 0
            0x0b, // end
        ]);

        crate::Module::new(&engine, wasm).unwrap();
    }

    #[test]
    fn module_compilation_accepts_transaction_object_helper_lowering() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        crate::Module::new(
            &engine,
            wat::parse_str(
                r#"
                (module
                  (type $s (tstruct (field (mut i32))))
                  (type $ps (tstruct (field (mut i8))))
                  (type $a (tarray (mut i32)))
                  (type $pa (tarray (mut i8)))
                  (type $ra (tarray (mut (tref $s))))
                  (data $d "\00\01\02\03")
                  (elem $e (tref $s)
                    (tstruct.new $s (i32.const 1))
                    (tstruct.new $s (i32.const 2)))
                  (tfunc (export "object-smoke") (result i32)
                    (local $sref (tref $s))
                    (local $psref (tref $ps))
                    (local $aref (tref $a))
                    (local $aref2 (tref $a))
                    (local $paref (tref $pa))
                    (local $raref (tref $ra))
                    (local.set $sref
                      (tstruct.new $s (i32.const 41)))
                    (drop (tstruct.new_default $s))
                    (local.set $psref
                      (tstruct.new $ps (i32.const -1)))
                    (drop (tstruct.get_s $ps 0 (tref.cast_read (local.get $psref))))
                    (drop (tstruct.get_u $ps 0 (tref.cast_read (local.get $psref))))
                    (tstruct.set $s 0 (tref.cast_write (local.get $sref)) (i32.const 42))
                    (local.set $aref
                      (tarray.new $a (i32.const 7) (i32.const 4)))
                    (local.set $aref2
                      (tarray.new_default $a (i32.const 4)))
                    (drop (tarray.new_fixed $a 2 (i32.const 1) (i32.const 2)))
                    (local.set $paref
                      (tarray.new_data $pa $d (i32.const 0) (i32.const 4)))
                    (local.set $raref
                      (tarray.new_elem $ra $e (i32.const 0) (i32.const 2)))
                    (tarray.set $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 1)
                      (i31.get_s (tref.ti31 (i32.const 13))))
                    (drop (tarray.len (local.get $aref)))
                    (drop (tarray.get_s $pa (tref.cast_read (local.get $paref)) (i32.const 0)))
                    (drop (tarray.get_u $pa (tref.cast_read (local.get $paref)) (i32.const 0)))
                    (tarray.fill $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 2)
                      (i32.const 5)
                      (i32.const 1))
                    (tarray.copy $a $a
                      (tref.cast_write (local.get $aref))
                      (i32.const 3)
                      (tref.cast_read (local.get $aref2))
                      (i32.const 0)
                      (i32.const 1))
                    (tarray.init_data $pa $d
                      (tref.cast_write (local.get $paref))
                      (i32.const 1)
                      (i32.const 0)
                      (i32.const 1))
                    (tarray.init_elem $ra $e
                      (tref.cast_write (local.get $raref))
                      (i32.const 0)
                      (i32.const 0)
                      (i32.const 1))
                    (drop (ti31.get_u (tref.ti31 (i32.const 13))))
                    (drop (tany.convert_textern
                      (textern.convert_tany (tref.ti31 (i32.const 7)))))
                    (i32.add
                      (tstruct.get $s 0 (tref.cast_read (local.get $sref)))
                      (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))))
                "#,
            )
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn module_compilation_rejects_transaction_i31_reference_fields_for_now() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let error = crate::Module::new(
            &engine,
            wat::parse_str(
                r#"
                (module
                  (type $s (struct (field (mut (ref null i31)))))
                  (tfunc (export "x")
                    (drop (tstruct.new_default $s))))
                "#,
            )
            .unwrap(),
        )
        .unwrap_err();
        let error = format!("{error:?}");
        assert!(
            error.contains("transactional i31 reference ObjectId values are not implemented yet"),
            "{error}"
        );
    }

    #[test]
    fn transaction_object_tstruct_executes_through_object_table() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $s (tstruct (field (mut i32))))
              (tfunc (export "create_set_get") (result i32)
                (local $sref (tref $s))
                (local.set $sref
                  (tstruct.new $s (i32.const 41)))
                (tstruct.set $s 0 (tref.cast_write (local.get $sref)) (i32.const 42))
                (tstruct.get $s 0 (tref.cast_read (local.get $sref))))
              (tfunc (export "i31s") (result i32)
                (ti31.get_s (tref.ti31 (i32.const 0x7fffffff))))
              (tfunc (export "i31u") (result i32)
                (ti31.get_u (tref.ti31 (i32.const 0x7fffffff)))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let create_set_get = instance
            .get_typed_func::<(), i32>(&mut store, "create_set_get")
            .unwrap();
        let i31s = instance
            .get_typed_func::<(), i32>(&mut store, "i31s")
            .unwrap();
        let i31u = instance
            .get_typed_func::<(), i32>(&mut store, "i31u")
            .unwrap();

        assert_eq!(create_set_get.call(&mut store, ()).unwrap(), 42);
        assert_eq!(i31s.call(&mut store, ()).unwrap(), -1);
        assert_eq!(i31u.call(&mut store, ()).unwrap(), 0x7fffffff);
        assert_eq!(store.transaction_object_table().live_count(), 1);
        assert_eq!(
            store
                .transaction_object_table()
                .payload(ObjectId { object_index: 0 })
                .unwrap(),
            ObjectPayload::Struct(vec![ObjectValue::I32(42)])
        );
    }

    #[test]
    fn transaction_object_tstruct_tfail_frees_new_object_record() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $s (struct (field (mut i32))))
              (tfunc (export "create_fail")
                (drop (tstruct.new $s (i32.const 41)))
                (tfail)))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let create_fail = instance
            .get_typed_func::<(), ()>(&mut store, "create_fail")
            .unwrap();

        create_fail.call(&mut store, ()).unwrap();

        assert_eq!(store.transaction_object_table().live_count(), 0);
    }

    #[test]
    fn transaction_object_tarray_executes_through_object_table() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $a (tarray (mut i32)))
              (tfunc (export "create_set_get") (result i32)
                (local $aref (tref $a))
                (local.set $aref
                  (tarray.new $a (i32.const 7) (i32.const 3)))
                (tarray.set $a (tref.cast_write (local.get $aref)) (i32.const 1) (i32.const 42))
                (i32.add
                  (tarray.len (local.get $aref))
                  (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let create_set_get = instance
            .get_typed_func::<(), i32>(&mut store, "create_set_get")
            .unwrap();

        assert_eq!(create_set_get.call(&mut store, ()).unwrap(), 45);
        assert_eq!(store.transaction_object_table().live_count(), 1);
        assert_eq!(
            store
                .transaction_object_table()
                .payload(ObjectId { object_index: 0 })
                .unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::I32(7),
                ObjectValue::I32(42),
                ObjectValue::I32(7)
            ])
        );
    }

    #[test]
    fn transaction_object_tarray_default_and_fixed_constructors_create_object_records() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $a (tarray (mut f32)))
              (tfunc (export "default_get") (result f32)
                (local $aref (tref $a))
                (local.set $aref (tarray.new_default $a (i32.const 2)))
                (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 1)))
              (tfunc (export "fixed_get") (result f32)
                (local $aref (tref $a))
                (local.set $aref
                  (tarray.new_fixed $a 3
                    (f32.const 1)
                    (f32.const 2)
                    (f32.const 3)))
                (tarray.get $a (tref.cast_read (local.get $aref)) (i32.const 2))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let default_get = instance
            .get_typed_func::<(), f32>(&mut store, "default_get")
            .unwrap();
        let fixed_get = instance
            .get_typed_func::<(), f32>(&mut store, "fixed_get")
            .unwrap();

        assert_eq!(default_get.call(&mut store, ()).unwrap().to_bits(), 0);
        assert_eq!(
            fixed_get.call(&mut store, ()).unwrap().to_bits(),
            3.0f32.to_bits()
        );
        assert_eq!(store.transaction_object_table().live_count(), 2);
        assert_eq!(
            store
                .transaction_object_table()
                .payload(ObjectId { object_index: 0 })
                .unwrap(),
            ObjectPayload::Array(vec![ObjectValue::F32(0), ObjectValue::F32(0)])
        );
        assert_eq!(
            store
                .transaction_object_table()
                .payload(ObjectId { object_index: 1 })
                .unwrap(),
            ObjectPayload::Array(vec![
                ObjectValue::F32(1.0f32.to_bits()),
                ObjectValue::F32(2.0f32.to_bits()),
                ObjectValue::F32(3.0f32.to_bits())
            ])
        );
    }

    #[test]
    fn exported_tfunc_returning_tarray_ref_enters_transaction() {
        let mut config = crate::Config::new();
        config.wasm_gc(true);
        let engine = crate::Engine::new(&config).unwrap();
        let module = transaction_test_module(
            &engine,
            r#"
            (module
              (type $a (array (mut f32)))
              (tfunc $new (export "new") (result (ref $a))
                (tarray.new_default $a (i32.const 2)))
              (tfunc (export "read") (result f32)
                (tarray.get $a (tref.cast_read (tcall $new)) (i32.const 1))))
            "#,
        );
        let mut store = crate::Store::new(&engine, ());
        let instance = crate::Instance::new(&mut store, &module, &[]).unwrap();
        let new = instance.get_func(&mut store, "new").unwrap();
        let read = instance
            .get_typed_func::<(), f32>(&mut store, "read")
            .unwrap();
        let mut results = [crate::Val::null_any_ref()];

        new.call(&mut store, &[], &mut results).unwrap();
        assert_eq!(read.call(&mut store, ()).unwrap().to_bits(), 0);
        assert_eq!(store.transaction_object_table().live_count(), 2);
    }
}
