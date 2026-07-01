use crate::prelude::*;
use crate::runtime::store::InstanceId;
use crate::runtime::vm::block_region::{BLOCK_SIZE, MappedRegionSource};
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::path::Path;

use super::object_value::{
    FIRST_TRANSACTION_OBJECT_REF_HANDLE, TRANSACTION_OBJECT_REF_HANDLE_STEP,
    VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED, object_kind_from_u16,
};
use super::type_layout::{
    PersistentTypeKind, PersistentTypeLayout, TraceSlotKind, TypeLayoutId, TypeLayoutRegistry,
};
use super::wasmtime_layout::{
    WasmtimePersistentFieldLayout, persistent_layout_for_wasmtime_struct_type,
    validate_wasmtime_array_element_layout, validate_wasmtime_struct_field_layouts,
    wasmtime_array_layout_fingerprint, wasmtime_array_layout_from_element_layout,
    wasmtime_struct_layout_fingerprint, wasmtime_type_layout_namespace,
};
use super::{
    DanglingObjectRef, DanglingObjectRefKind, GranuleId, OBJECT_VALUE_ABI_LIVE_REF_KIND_GC,
    OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT, ObjectId, ObjectKind, ObjectPayload,
    ObjectValue, PersistentMarkSweepReport, PersistentObjectDirectoryEntry,
    PersistentObjectMarkReport, PersistentObjectMarker, PersistentObjectRecordLocation,
    PersistentObjectRecordSource, PersistentObjectRefRaw, PersistentRecoveryGcReport,
    PersistentRootError, PersistentRootErrorKind, PersistentVolatileSweepReport,
    TransactionObjectRefRaw, TransactionRegionRuntime, TxObjectHeader, object_gc, object_heap,
    persist,
};
use wasmtime_environ::VMSharedTypeIndex;

#[cfg(test)]
use super::wasmtime_layout::{
    PERSISTENT_OBJECT_ABI_SLOT_SIZE, wasmtime_struct_field_layouts_from_values,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ObjectTableSlot {
    pub(crate) kind: ObjectKind,
    pub(crate) version: u64,
    pub(crate) type_layout_id: u32,
    pub(crate) runtime_type_index: Option<VMSharedTypeIndex>,
    pub(crate) persistent: bool,
    pub(crate) current_record: object_heap::TxRecordHandle,
}

#[derive(Clone, Debug)]
pub(crate) struct InstalledPersistentObjectPublication {
    pub(crate) object_id: ObjectId,
    pub(crate) directory_entry: PersistentObjectDirectoryEntry,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WasmtimeTypeLayoutKey {
    namespace: u32,
    type_index: u32,
    kind: PersistentTypeKind,
    fingerprint: u64,
}

#[derive(Debug)]
pub(crate) struct ObjectTable {
    pub(crate) slots: Vec<Option<ObjectTableSlot>>,
    pub(crate) free_list: Vec<ObjectId>,
    pub(crate) transaction_ref_handles_to_objects: BTreeMap<u32, ObjectId>,
    pub(crate) objects_to_transaction_ref_handles: BTreeMap<ObjectId, u32>,
    pub(crate) next_transaction_ref_handle: u32,
    // SHISOFT-TWASM-MOCK: live transaction ref bridge; persistent object records
    // must use ObjectId and must not call this helper.
    pub(crate) live_bridge_gc_refs_to_objects: BTreeMap<u32, ObjectId>,
    pub(crate) object_to_live_bridge_gc_ref: BTreeMap<ObjectId, u32>,
    pub(crate) type_layouts: TypeLayoutRegistry,
    pub(crate) wasmtime_type_layout_ids: BTreeMap<WasmtimeTypeLayoutKey, TypeLayoutId>,
    pub(crate) next_dynamic_type_layout_id: Option<u32>,
    pub(crate) next_version: u64,
    pub(crate) next_record_version: u32,
    pub(crate) live_count: usize,
    pub(crate) heap: object_heap::ObjectHeap,
    shared_region_runtime: Option<TransactionRegionRuntime>,
}

impl Default for ObjectTable {
    fn default() -> Self {
        let mut table = Self {
            slots: Vec::new(),
            free_list: Vec::new(),
            transaction_ref_handles_to_objects: BTreeMap::new(),
            objects_to_transaction_ref_handles: BTreeMap::new(),
            next_transaction_ref_handle: FIRST_TRANSACTION_OBJECT_REF_HANDLE,
            live_bridge_gc_refs_to_objects: BTreeMap::new(),
            object_to_live_bridge_gc_ref: BTreeMap::new(),
            type_layouts: TypeLayoutRegistry::default(),
            wasmtime_type_layout_ids: BTreeMap::new(),
            next_dynamic_type_layout_id: Some(TypeLayoutId::BUILTIN_FUNC.get() + 1),
            next_version: 0,
            next_record_version: 0,
            live_count: 0,
            heap: object_heap::ObjectHeap::default(),
            shared_region_runtime: None,
        };
        for layout in builtin_type_layouts() {
            table
                .register_type_layout(layout)
                .expect("built-in type layouts must register");
        }
        table
    }
}

impl ObjectTable {
    pub(crate) fn is_raw_i31_ref(raw_ref: u64) -> bool {
        raw_ref <= u64::from(u32::MAX) && (raw_ref & 1) == 1
    }

    pub(crate) fn decode_raw_i31_ref(raw_ref: u64) -> Result<i32> {
        ensure!(
            Self::is_raw_i31_ref(raw_ref),
            "raw ref is not an i31 immediate"
        );
        Ok((raw_ref as u32 as i32) >> 1)
    }

    pub(crate) fn encode_raw_i31_ref(value: i32) -> u64 {
        u64::from(((value as u32) << 1) | 1)
    }

    pub(crate) fn install_recovered_type_layouts(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
    ) -> Result<()> {
        self.type_layouts = TypeLayoutRegistry::default();
        self.wasmtime_type_layout_ids.clear();
        self.next_dynamic_type_layout_id = Some(TypeLayoutId::BUILTIN_FUNC.get() + 1);
        for layout in builtin_type_layouts() {
            self.register_type_layout(layout)?;
        }
        self.merge_recovered_type_layouts(recovered_type_layouts)
    }

    pub(crate) fn merge_recovered_type_layouts(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
    ) -> Result<()> {
        for layout in recovered_type_layouts.iter().cloned() {
            if is_builtin_type_layout_id(layout.id()) {
                continue;
            }
            self.register_type_layout(layout)?;
        }
        Ok(())
    }

    pub(crate) fn persistent_type_layout_id_for_wasmtime_key(
        &mut self,
        namespace: u32,
        type_index: u32,
        kind: PersistentTypeKind,
        fingerprint: u64,
    ) -> Result<TypeLayoutId> {
        let key = WasmtimeTypeLayoutKey {
            namespace,
            type_index,
            kind,
            fingerprint,
        };
        if let Some(id) = self.wasmtime_type_layout_ids.get(&key).copied() {
            return Ok(id);
        }

        if let Some(id) = self
            .type_layouts
            .ids_for_fingerprint(fingerprint)
            .find(|id| {
                self.type_layouts
                    .get(*id)
                    .is_some_and(|layout| layout.kind() == kind)
            })
        {
            self.wasmtime_type_layout_ids.insert(key, id);
            return Ok(id);
        }

        let id = self.allocate_dynamic_type_layout_id()?;
        self.wasmtime_type_layout_ids.insert(key, id);
        Ok(id)
    }

    pub(crate) fn allocate_dynamic_type_layout_id(&mut self) -> Result<TypeLayoutId> {
        loop {
            let raw = self
                .next_dynamic_type_layout_id
                .context("transactional persistent type layout id space is exhausted")?;
            let next = raw.checked_add(1);
            self.next_dynamic_type_layout_id = next;
            let id = TypeLayoutId::new(raw)
                .context("dynamic persistent type layout id cannot be zero")?;
            if !self.type_layouts.contains(id) {
                return Ok(id);
            }
        }
    }

    pub(crate) fn observe_type_layout_id(&mut self, id: TypeLayoutId) {
        self.next_dynamic_type_layout_id = match self.next_dynamic_type_layout_id {
            None => None,
            Some(next) => match id.get().checked_add(1) {
                Some(candidate) => Some(next.max(candidate)),
                None => None,
            },
        };
    }

    pub(crate) fn allocate(&mut self, kind: ObjectKind) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::default_for_kind(kind)?)
    }

    pub(crate) fn allocate_struct(&mut self, fields: Vec<ObjectValue>) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Struct(fields))
    }

    pub(crate) fn allocate_persistent_struct_for_gc_ref_with_wasmtime_type_layout(
        &mut self,
        gc_ref: u32,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        field_layouts: Vec<WasmtimePersistentFieldLayout>,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let namespace = wasmtime_type_layout_namespace(owner_instance)?;
        self.allocate_persistent_struct_for_gc_ref_with_wasmtime_type_layout_namespace(
            gc_ref,
            namespace,
            type_index,
            field_layouts,
            fields,
        )
    }

    pub(crate) fn allocate_persistent_struct_for_gc_ref_with_wasmtime_type_layout_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        field_layouts: Vec<WasmtimePersistentFieldLayout>,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            field_layouts.len() == fields.len(),
            "transactional persistent struct layout field count does not match payload field count"
        );
        let body_size = validate_wasmtime_struct_field_layouts(&field_layouts)?;
        let fingerprint = wasmtime_struct_layout_fingerprint(body_size, &field_layouts)?;
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Struct,
            fingerprint,
        )?;
        let layout =
            persistent_layout_for_wasmtime_struct_type(type_layout_id, body_size, &field_layouts)?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_struct_for_gc_ref_with_type_layout_id(gc_ref, fields, layout.id())
    }

    pub(crate) fn allocate_persistent_struct_with_wasmtime_type_layout(
        &mut self,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        field_layouts: Vec<WasmtimePersistentFieldLayout>,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let namespace = wasmtime_type_layout_namespace(owner_instance)?;
        self.allocate_persistent_struct_with_wasmtime_type_layout_namespace(
            namespace,
            type_index,
            field_layouts,
            fields,
        )
    }

    pub(crate) fn allocate_persistent_struct_with_wasmtime_type_layout_namespace(
        &mut self,
        type_namespace: u32,
        type_index: u32,
        field_layouts: Vec<WasmtimePersistentFieldLayout>,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        ensure!(
            field_layouts.len() == fields.len(),
            "transactional persistent struct layout field count does not match payload field count"
        );
        let body_size = validate_wasmtime_struct_field_layouts(&field_layouts)?;
        let fingerprint = wasmtime_struct_layout_fingerprint(body_size, &field_layouts)?;
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Struct,
            fingerprint,
        )?;
        let layout =
            persistent_layout_for_wasmtime_struct_type(type_layout_id, body_size, &field_layouts)?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_struct_with_type_layout_id(fields, layout.id())
    }

    pub(crate) fn ensure_persistent_struct_layout_for_wasmtime_type_layout_namespace(
        &mut self,
        type_namespace: u32,
        type_index: u32,
        field_layouts: Vec<WasmtimePersistentFieldLayout>,
    ) -> Result<TypeLayoutId> {
        let body_size = validate_wasmtime_struct_field_layouts(&field_layouts)?;
        let fingerprint = wasmtime_struct_layout_fingerprint(body_size, &field_layouts)?;
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Struct,
            fingerprint,
        )?;
        let layout =
            persistent_layout_for_wasmtime_struct_type(type_layout_id, body_size, &field_layouts)?;
        self.register_type_layout(layout.clone())?;
        Ok(layout.id())
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_struct_for_gc_ref_with_wasmtime_type_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let (body_size, field_layouts) = wasmtime_struct_field_layouts_from_values(&fields)?;
        let fingerprint = wasmtime_struct_layout_fingerprint(body_size, &field_layouts)?;
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Struct,
            fingerprint,
        )?;
        let layout =
            persistent_layout_for_wasmtime_struct_type(type_layout_id, body_size, &field_layouts)?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_struct_for_gc_ref_with_type_layout_id(gc_ref, fields, layout.id())
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_struct_for_gc_ref(
        &mut self,
        gc_ref: u32,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        self.allocate_persistent_struct_for_gc_ref_with_type_layout_id(
            gc_ref,
            fields,
            TypeLayoutId::DEFAULT_STRUCT,
        )
    }

    pub(crate) fn allocate_persistent_struct_for_gc_ref_with_type_layout_id(
        &mut self,
        gc_ref: u32,
        fields: Vec<ObjectValue>,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.ensure_live_gc_ref_bridge_available(gc_ref, "struct")?;
        let object_id = self.allocate_payload_with_type_layout_id(
            ObjectPayload::Struct(fields),
            type_layout_id,
            true,
        )?;
        self.associate_live_gc_ref_for_transaction_bridge(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_persistent_struct_with_type_layout_id(
        &mut self,
        fields: Vec<ObjectValue>,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.allocate_payload_with_type_layout_id(
            ObjectPayload::Struct(fields),
            type_layout_id,
            true,
        )
    }

    pub(crate) fn allocate_struct_for_gc_ref(
        &mut self,
        gc_ref: u32,
        fields: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        self.ensure_live_gc_ref_bridge_available(gc_ref, "struct")?;
        let object_id = self.allocate_struct(fields)?;
        self.associate_live_gc_ref_for_transaction_bridge(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_array(&mut self, elements: Vec<ObjectValue>) -> Result<ObjectId> {
        self.allocate_payload(ObjectPayload::Array(elements))
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_type(
        &mut self,
        gc_ref: u32,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let namespace = wasmtime_type_layout_namespace(owner_instance)?;
        self.allocate_persistent_array_for_gc_ref_with_wasmtime_type_namespace(
            gc_ref, namespace, type_index, elements,
        )
    }

    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_element_layout_and_initializer(
        &mut self,
        gc_ref: u32,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let namespace = wasmtime_type_layout_namespace(owner_instance)?;
        self.allocate_persistent_array_for_gc_ref_with_wasmtime_element_layout_and_initializer_namespace(
            gc_ref,
            namespace,
            type_index,
            element_size,
            element_is_object_ref,
            initializer,
            len,
        )
    }

    pub(crate) fn allocate_persistent_array_with_wasmtime_element_layout_and_initializer(
        &mut self,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let namespace = wasmtime_type_layout_namespace(owner_instance)?;
        self.allocate_persistent_array_with_wasmtime_element_layout_and_initializer_namespace(
            namespace,
            type_index,
            element_size,
            element_is_object_ref,
            initializer,
            len,
        )
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_type_and_initializer(
        &mut self,
        gc_ref: u32,
        owner_instance: Option<InstanceId>,
        type_index: u32,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let element_is_object_ref = matches!(initializer, ObjectValue::Ref(_));
        self.allocate_persistent_array_for_gc_ref_with_wasmtime_element_layout_and_initializer(
            gc_ref,
            owner_instance,
            type_index,
            PERSISTENT_OBJECT_ABI_SLOT_SIZE,
            element_is_object_ref,
            initializer,
            len,
        )
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_type_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let element_is_object_ref = elements
            .iter()
            .any(|value| matches!(value, ObjectValue::Ref(_)));
        self.allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
            gc_ref,
            type_namespace,
            type_index,
            PERSISTENT_OBJECT_ABI_SLOT_SIZE,
            element_is_object_ref,
            elements,
        )
    }

    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_fixed_type_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let element_size =
            validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?;
        let fingerprint = wasmtime_array_layout_fingerprint(element_size, element_is_object_ref);
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Array,
            fingerprint,
        )?;
        let layout = wasmtime_array_layout_from_element_layout(
            type_layout_id,
            element_size,
            element_is_object_ref,
        )?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_array_for_gc_ref_with_type_layout_id(gc_ref, elements, layout.id())
    }

    pub(crate) fn allocate_persistent_array_with_wasmtime_fixed_type_namespace(
        &mut self,
        type_namespace: u32,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        let element_size =
            validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?;
        let fingerprint = wasmtime_array_layout_fingerprint(element_size, element_is_object_ref);
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Array,
            fingerprint,
        )?;
        let layout = wasmtime_array_layout_from_element_layout(
            type_layout_id,
            element_size,
            element_is_object_ref,
        )?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_array_with_type_layout_id(elements, layout.id())
    }

    pub(crate) fn ensure_persistent_array_layout_for_wasmtime_type_layout_namespace(
        &mut self,
        type_namespace: u32,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
    ) -> Result<TypeLayoutId> {
        let element_size =
            validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?;
        let fingerprint = wasmtime_array_layout_fingerprint(element_size, element_is_object_ref);
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Array,
            fingerprint,
        )?;
        let layout = wasmtime_array_layout_from_element_layout(
            type_layout_id,
            element_size,
            element_is_object_ref,
        )?;
        self.register_type_layout(layout.clone())?;
        Ok(layout.id())
    }

    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_element_layout_and_initializer_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let element_size =
            validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?;
        let fingerprint = wasmtime_array_layout_fingerprint(element_size, element_is_object_ref);
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Array,
            fingerprint,
        )?;
        let layout = wasmtime_array_layout_from_element_layout(
            type_layout_id,
            element_size,
            element_is_object_ref,
        )?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_array_for_gc_ref_with_type_layout_id(
            gc_ref,
            vec![initializer; len],
            layout.id(),
        )
    }

    pub(crate) fn allocate_persistent_array_with_wasmtime_element_layout_and_initializer_namespace(
        &mut self,
        type_namespace: u32,
        type_index: u32,
        element_size: u32,
        element_is_object_ref: bool,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let element_size =
            validate_wasmtime_array_element_layout(element_size, element_is_object_ref)?;
        let fingerprint = wasmtime_array_layout_fingerprint(element_size, element_is_object_ref);
        let type_layout_id = self.persistent_type_layout_id_for_wasmtime_key(
            type_namespace,
            type_index,
            PersistentTypeKind::Array,
            fingerprint,
        )?;
        let layout = wasmtime_array_layout_from_element_layout(
            type_layout_id,
            element_size,
            element_is_object_ref,
        )?;
        self.register_type_layout(layout.clone())?;
        self.allocate_persistent_array_with_type_layout_id(vec![initializer; len], layout.id())
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_array_for_gc_ref_with_wasmtime_type_and_initializer_namespace(
        &mut self,
        gc_ref: u32,
        type_namespace: u32,
        type_index: u32,
        initializer: ObjectValue,
        len: usize,
    ) -> Result<ObjectId> {
        let element_is_object_ref = matches!(initializer, ObjectValue::Ref(_));
        self.allocate_persistent_array_for_gc_ref_with_wasmtime_element_layout_and_initializer_namespace(
            gc_ref,
            type_namespace,
            type_index,
            PERSISTENT_OBJECT_ABI_SLOT_SIZE,
            element_is_object_ref,
            initializer,
            len,
        )
    }

    #[cfg(test)]
    pub(crate) fn allocate_persistent_array_for_gc_ref(
        &mut self,
        gc_ref: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        self.allocate_persistent_array_for_gc_ref_with_type_layout_id(
            gc_ref,
            elements,
            TypeLayoutId::DEFAULT_ARRAY,
        )
    }

    pub(crate) fn allocate_persistent_array_for_gc_ref_with_type_layout_id(
        &mut self,
        gc_ref: u32,
        elements: Vec<ObjectValue>,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.ensure_live_gc_ref_bridge_available(gc_ref, "array")?;
        let object_id = self.allocate_payload_with_type_layout_id(
            ObjectPayload::Array(elements),
            type_layout_id,
            true,
        )?;
        self.associate_live_gc_ref_for_transaction_bridge(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn allocate_persistent_array_with_type_layout_id(
        &mut self,
        elements: Vec<ObjectValue>,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.allocate_payload_with_type_layout_id(
            ObjectPayload::Array(elements),
            type_layout_id,
            true,
        )
    }

    pub(crate) fn allocate_array_for_gc_ref(
        &mut self,
        gc_ref: u32,
        elements: Vec<ObjectValue>,
    ) -> Result<ObjectId> {
        self.ensure_live_gc_ref_bridge_available(gc_ref, "array")?;
        let object_id = self.allocate_array(elements)?;
        self.associate_live_gc_ref_for_transaction_bridge(gc_ref, object_id)?;
        Ok(object_id)
    }

    pub(crate) fn live_bridge_object_id_for_raw_ref_or_func(
        &mut self,
        raw_ref: u64,
    ) -> Result<ObjectId> {
        ensure!(raw_ref != 0, "transactional object cannot use null ref");
        if let Ok(gc_ref) = u32::try_from(raw_ref)
            && let Some(object_id) = self.live_bridge_gc_refs_to_objects.get(&gc_ref).copied()
        {
            return Ok(object_id);
        }
        if let Some(object_id) = self.persistent_object_id_for_raw_ref(raw_ref)? {
            return Ok(object_id);
        }
        if Self::is_raw_i31_ref(raw_ref) {
            bail!("raw i31 refs are inline durable values, not object-table payloads");
        }
        bail!("transactional function reference promotion requires durable function identity")
    }

    pub(crate) fn allocate_payload(&mut self, payload: ObjectPayload) -> Result<ObjectId> {
        self.allocate_payload_with_persistence(payload, false)
    }

    pub(crate) fn allocate_payload_with_persistence(
        &mut self,
        payload: ObjectPayload,
        persistent: bool,
    ) -> Result<ObjectId> {
        let type_layout_id = default_type_layout_id_for_kind(payload.kind());
        self.allocate_payload_with_type_layout_id(payload, type_layout_id, persistent)
    }

    pub(crate) fn allocate_payload_with_type_layout_id(
        &mut self,
        payload: ObjectPayload,
        type_layout_id: TypeLayoutId,
        persistent: bool,
    ) -> Result<ObjectId> {
        self.validate_type_layout_for_object_kind(payload.kind(), type_layout_id)?;

        let shared_persistent = persistent && self.shared_region_runtime.is_some();
        let reused_slot = (!shared_persistent)
            .then(|| self.free_list.last().copied())
            .flatten();
        let object_id = match reused_slot {
            Some(object_id) => object_id,
            None => {
                if persistent {
                    if let Some(runtime) = &self.shared_region_runtime {
                        runtime.allocate_persistent_object_id_at_least(
                            u64::try_from(self.slots.len())
                                .context("object table slot count does not fit u64")?,
                        )?
                    } else {
                        ObjectId {
                            object_index: u64::try_from(self.slots.len())
                                .context("object table slot count does not fit u64")?,
                        }
                    }
                } else {
                    ObjectId {
                        object_index: u64::try_from(self.slots.len())
                            .context("object table slot count does not fit u64")?,
                    }
                }
            }
        };
        let index = object_slot_index(object_id)?;
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
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
        if persistent {
            if let Some(runtime) = &self.shared_region_runtime {
                runtime.reserve_persistent_object_metadata(
                    object_id,
                    kind,
                    type_layout_id.get(),
                )?;
            }
        }
        let record_version = self.bump_record_version()?;
        let record = self.heap.allocate_record(
            object_id,
            record_version,
            kind,
            0,
            type_layout_id.get(),
            &payload,
        )?;
        let version = self.bump_object_version()?;
        if reused_slot.is_some() {
            let _ = self.free_list.pop();
        }
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_layout_id: type_layout_id.get(),
            runtime_type_index: None,
            persistent,
            current_record: record,
        });
        self.live_count = self
            .live_count
            .checked_add(1)
            .context("object table live count overflow")?;
        Ok(object_id)
    }

    pub(crate) fn reserve_persistent_object_id_for_promotion(
        &mut self,
        kind: ObjectKind,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.allocate_payload_with_type_layout_id(
            ObjectPayload::default_for_kind(kind)?,
            type_layout_id,
            true,
        )
    }

    pub(crate) fn fill_reserved_persistent_object_for_promotion(
        &mut self,
        object_id: ObjectId,
        payload: ObjectPayload,
    ) -> Result<()> {
        ensure!(
            self.is_persistent(object_id)?,
            "promotion target object must be persistent"
        );
        ensure!(
            self.kind(object_id)? == payload.kind(),
            "promotion target object kind does not match promoted payload"
        );
        self.validate_persistent_payload_refs(&payload)?;
        self.update_payload(object_id, payload)
    }

    pub(crate) fn register_type_layout(&mut self, layout: PersistentTypeLayout) -> Result<()> {
        let id = layout.id();
        self.type_layouts.insert(layout)?;
        self.observe_type_layout_id(id);
        Ok(())
    }

    pub(crate) fn type_layouts(&self) -> &TypeLayoutRegistry {
        &self.type_layouts
    }

    pub(crate) fn require_type_layout(&self, id: TypeLayoutId) -> Result<&PersistentTypeLayout> {
        self.type_layouts
            .get(id)
            .with_context(|| format!("unknown persistent type layout id: {}", id.get()))
    }

    pub(crate) fn live_count(&self) -> usize {
        self.live_count
    }

    pub(crate) fn set_shared_region_runtime(&mut self, runtime: Option<TransactionRegionRuntime>) {
        self.shared_region_runtime = runtime;
    }

    pub(crate) fn refresh_persistent_object_from_shared_directory(
        &mut self,
        object_id: ObjectId,
    ) -> Result<bool> {
        let Some(runtime) = &self.shared_region_runtime else {
            return Ok(false);
        };
        let Some(entry) = runtime.persistent_object_directory_entry(object_id)? else {
            return Ok(false);
        };

        if let Ok(slot) = self.live_slot(object_id) {
            if !slot.persistent || slot.version >= entry.directory_version {
                return Ok(false);
            }
        }

        TypeLayoutId::new(entry.type_layout_id)
            .context("persistent object directory entry type layout id cannot be zero")?;
        self.merge_recovered_type_layouts(&runtime.recovered_type_layouts()?)?;

        self.install_persistent_object_directory_entry(entry)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn live_object_ids_for_test(&self) -> Vec<ObjectId> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                slot.as_ref().map(|_| ObjectId {
                    object_index: u64::try_from(index).unwrap(),
                })
            })
            .collect()
    }

    pub(crate) fn ensure_live_gc_ref_bridge_available(
        &self,
        gc_ref: u32,
        object_kind: &str,
    ) -> Result<()> {
        ensure!(
            gc_ref != 0,
            "transactional {object_kind} object cannot use null GC ref"
        );
        ensure!(
            !self.live_bridge_gc_refs_to_objects.contains_key(&gc_ref),
            "transactional object GC ref is already associated"
        );
        ensure!(
            !self
                .transaction_ref_handles_to_objects
                .contains_key(&gc_ref),
            "transactional object GC ref collides with transaction object handle"
        );
        Ok(())
    }

    pub(crate) fn associate_live_gc_ref_for_transaction_bridge(
        &mut self,
        gc_ref: u32,
        object_id: ObjectId,
    ) -> Result<()> {
        ensure!(gc_ref != 0, "transactional object cannot use null GC ref");
        self.live_slot(object_id)?;
        ensure!(
            !self
                .transaction_ref_handles_to_objects
                .contains_key(&gc_ref),
            "transactional object GC ref collides with transaction object handle"
        );
        if let Some(existing) = self.live_bridge_gc_refs_to_objects.get(&gc_ref).copied() {
            ensure!(
                existing == object_id,
                "transactional object GC ref is already associated"
            );
        }
        if let Some(existing) = self.object_to_live_bridge_gc_ref.get(&object_id).copied() {
            ensure!(
                existing == gc_ref,
                "transactional object id is already associated with another GC ref"
            );
        }
        self.live_bridge_gc_refs_to_objects
            .insert(gc_ref, object_id);
        self.object_to_live_bridge_gc_ref.insert(object_id, gc_ref);
        Ok(())
    }

    pub(crate) fn object_id_for_live_gc_ref_bridge(&self, gc_ref: u32) -> Result<ObjectId> {
        ensure!(gc_ref != 0, "transactional object cannot use null GC ref");
        self.live_bridge_gc_refs_to_objects
            .get(&gc_ref)
            .copied()
            .with_context(|| format!("unknown transactional object GC ref: {gc_ref:#x}"))
    }

    pub(crate) fn transaction_ref_handle_for_object_id(
        &mut self,
        object_id: ObjectId,
    ) -> Result<u32> {
        self.transaction_ref_handle_for_object_id_avoiding(object_id, |_| false)
    }

    pub(crate) fn transaction_ref_handle_for_object_id_avoiding<F>(
        &mut self,
        object_id: ObjectId,
        is_reserved_live_ref_raw: F,
    ) -> Result<u32>
    where
        F: Fn(u32) -> bool,
    {
        self.live_slot(object_id)?;
        if let Some(handle) = self
            .objects_to_transaction_ref_handles
            .get(&object_id)
            .copied()
        {
            ensure!(
                !is_reserved_live_ref_raw(handle),
                "transaction object ref handle collides with registered live ref"
            );
            return Ok(handle);
        }

        let start = if self.next_transaction_ref_handle == 0 {
            FIRST_TRANSACTION_OBJECT_REF_HANDLE
        } else {
            self.next_transaction_ref_handle
        };
        let mut candidate = start;
        loop {
            if !self
                .transaction_ref_handles_to_objects
                .contains_key(&candidate)
                && !self.live_bridge_gc_refs_to_objects.contains_key(&candidate)
                && !is_reserved_live_ref_raw(candidate)
            {
                self.transaction_ref_handles_to_objects
                    .insert(candidate, object_id);
                self.objects_to_transaction_ref_handles
                    .insert(object_id, candidate);
                self.next_transaction_ref_handle = candidate
                    .checked_add(TRANSACTION_OBJECT_REF_HANDLE_STEP)
                    .unwrap_or(TRANSACTION_OBJECT_REF_HANDLE_STEP);
                return Ok(candidate);
            }
            candidate = candidate
                .checked_add(TRANSACTION_OBJECT_REF_HANDLE_STEP)
                .unwrap_or(TRANSACTION_OBJECT_REF_HANDLE_STEP);
            ensure!(
                candidate != start,
                "transaction object ref handle space is exhausted"
            );
        }
    }

    pub(crate) fn object_id_for_transaction_ref_handle(&self, handle: u32) -> Result<ObjectId> {
        let handle = TransactionObjectRefRaw::from_handle(handle)?.as_raw();
        let object_id = self
            .transaction_ref_handles_to_objects
            .get(&handle)
            .copied()
            .with_context(|| format!("unknown transaction object ref handle: {handle:#x}"))?;
        self.live_slot(object_id)?;
        Ok(object_id)
    }

    pub(crate) fn known_persistent_object_id_for_transaction_ref_handle(
        &self,
        handle: u32,
    ) -> Result<Option<ObjectId>> {
        let Some(object_id) = self.known_object_id_for_transaction_ref_handle(handle) else {
            return Ok(None);
        };
        if self.is_persistent(object_id)? {
            Ok(Some(object_id))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn known_object_id_for_transaction_ref_handle(
        &self,
        handle: u32,
    ) -> Option<ObjectId> {
        let handle = TransactionObjectRefRaw::from_raw(handle).decode()?;
        let object_id = self
            .transaction_ref_handles_to_objects
            .get(&handle)
            .copied()?;
        self.live_slot(object_id).ok()?;
        Some(object_id)
    }

    pub(crate) fn object_id_for_live_bridge_transaction_ref_raw(
        &self,
        raw_ref: u32,
    ) -> Result<ObjectId> {
        ensure!(raw_ref != 0, "transactional object cannot use null ref");
        if let Some(object_id) = self.known_object_id_for_transaction_ref_handle(raw_ref) {
            return Ok(object_id);
        }
        if let Some(object_id) = self.known_object_id_for_live_gc_ref_bridge(raw_ref) {
            return Ok(object_id);
        }
        if let Some(object_id) = self.persistent_object_id_for_raw_ref(u64::from(raw_ref))? {
            return Ok(object_id);
        }
        self.object_id_for_live_gc_ref_bridge(raw_ref)
    }

    pub(crate) fn known_object_id_for_live_gc_ref_bridge(&self, gc_ref: u32) -> Option<ObjectId> {
        if gc_ref == 0 {
            return None;
        }
        self.live_bridge_gc_refs_to_objects.get(&gc_ref).copied()
    }

    pub(crate) fn known_persistent_object_id_for_live_gc_ref_bridge(
        &self,
        gc_ref: u32,
    ) -> Result<Option<ObjectId>> {
        let Some(object_id) = self.known_object_id_for_live_gc_ref_bridge(gc_ref) else {
            return Ok(None);
        };
        if self.is_persistent(object_id)? {
            Ok(Some(object_id))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn known_persistent_object_id_for_live_bridge_transaction_ref_raw(
        &self,
        raw_ref: u32,
    ) -> Result<Option<ObjectId>> {
        if let Some(object_id) =
            self.known_persistent_object_id_for_transaction_ref_handle(raw_ref)?
        {
            return Ok(Some(object_id));
        }
        if let Some(object_id) = self.known_persistent_object_id_for_live_gc_ref_bridge(raw_ref)? {
            return Ok(Some(object_id));
        }
        self.persistent_object_id_for_raw_ref(u64::from(raw_ref))
    }

    pub(crate) fn persistent_object_id_for_live_bridge_or_promotion_required(
        &self,
        gc_ref: u32,
    ) -> Result<Option<ObjectId>> {
        if gc_ref == 0 {
            return Ok(None);
        }
        let Some(object_id) = self.known_object_id_for_live_gc_ref_bridge(gc_ref) else {
            bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
        };
        ensure!(
            self.is_persistent(object_id)?,
            VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED
        );
        Ok(Some(object_id))
    }

    pub(crate) fn live_bridge_gc_ref_lookup_for_object_id(
        &self,
        object_id: ObjectId,
    ) -> Result<u32> {
        self.live_slot(object_id)?;
        self.object_to_live_bridge_gc_ref
            .get(&object_id)
            .copied()
            .with_context(|| {
                format!("transactional object has no GC ref association: {object_id:?}")
            })
    }

    pub(crate) fn raw_ref_for_object_id(&self, object_id: ObjectId) -> Result<u64> {
        Ok(self.persistent_ref_abi_for_object_id(object_id)?.0)
    }

    pub(crate) fn persistent_ref_abi_for_object_id(
        &self,
        object_id: ObjectId,
    ) -> Result<(u64, u64)> {
        self.live_slot(object_id)?;
        ensure!(
            self.is_persistent(object_id)?,
            "persistent ref ABI requires a persistent object: {object_id:?}"
        );
        Ok((
            PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw(),
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        ))
    }

    pub(crate) fn persistent_ref_raw_for_live_bridge(object_id: ObjectId) -> Result<u64> {
        let raw = PersistentObjectRefRaw::from_optional_object_id(Some(object_id))?.as_raw();
        ensure!(
            u32::try_from(raw).is_ok(),
            "persistent object ref does not fit live transaction ref bridge"
        );
        Ok(raw)
    }

    pub(crate) fn live_bridge_ref_abi_for_object_id(
        &self,
        object_id: ObjectId,
    ) -> Result<(u64, u64)> {
        self.live_slot(object_id)?;
        if let Some(gc_ref) = self.object_to_live_bridge_gc_ref.get(&object_id).copied() {
            return Ok((u64::from(gc_ref), OBJECT_VALUE_ABI_LIVE_REF_KIND_GC));
        }
        self.persistent_ref_abi_for_object_id(object_id)?;
        Ok((
            Self::persistent_ref_raw_for_live_bridge(object_id)?,
            OBJECT_VALUE_ABI_LIVE_REF_KIND_PERSISTENT_OBJECT,
        ))
    }

    pub(crate) fn persistent_object_id_for_raw_ref(
        &self,
        raw_ref: u64,
    ) -> Result<Option<ObjectId>> {
        let Some(object_id) = PersistentObjectRefRaw::from_raw(raw_ref).decode() else {
            return Ok(None);
        };
        let Ok(slot) = self.live_slot(object_id) else {
            return Ok(None);
        };
        if slot.persistent {
            Ok(Some(object_id))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn kind(&self, object_id: ObjectId) -> Result<ObjectKind> {
        Ok(self.live_slot(object_id)?.kind)
    }

    pub(crate) fn runtime_type_index(
        &self,
        object_id: ObjectId,
    ) -> Result<Option<VMSharedTypeIndex>> {
        Ok(self.live_slot(object_id)?.runtime_type_index)
    }

    pub(crate) fn set_runtime_type_index(
        &mut self,
        object_id: ObjectId,
        runtime_type_index: VMSharedTypeIndex,
    ) -> Result<()> {
        let index = object_slot_index(object_id)?;
        let slot = self.live_slot(object_id)?.clone();
        self.slots[index] = Some(ObjectTableSlot {
            runtime_type_index: Some(runtime_type_index),
            ..slot
        });
        Ok(())
    }

    pub(crate) fn version(&self, object_id: ObjectId) -> Result<u64> {
        Ok(self.live_slot(object_id)?.version)
    }

    pub(crate) fn refreshed_version(&mut self, object_id: ObjectId) -> Result<u64> {
        self.refresh_persistent_object_from_shared_directory(object_id)?;
        self.version(object_id)
    }

    pub(crate) fn is_persistent(&self, object_id: ObjectId) -> Result<bool> {
        Ok(self.live_slot(object_id)?.persistent)
    }

    pub(crate) fn payload(&self, object_id: ObjectId) -> Result<ObjectPayload> {
        let slot = self.live_slot(object_id)?;
        self.heap.payload(slot.current_record)
    }

    pub(crate) fn refreshed_payload(&mut self, object_id: ObjectId) -> Result<ObjectPayload> {
        self.refresh_persistent_object_from_shared_directory(object_id)?;
        self.payload(object_id)
    }

    pub(crate) fn trace_object_ids(&self, object_id: ObjectId) -> Result<Vec<ObjectId>> {
        let slot = self.live_slot(object_id)?;
        if !slot.persistent {
            return self.heap.trace_object_ids(slot.current_record);
        }

        let type_layout_id = TypeLayoutId::new(slot.type_layout_id)
            .context("persistent object table slot type layout id cannot be zero")?;
        let layout = self.require_type_layout(type_layout_id)?;
        // Wave 7 still allocates persistent structs/arrays through builtin
        // placeholder layouts with no durable ref-slot metadata. Keep the
        // payload-derived path for those placeholders until Wave 8 starts
        // assigning real per-shape layouts during persistent allocation.
        if uses_placeholder_persistent_trace_fallback(type_layout_id) {
            return self.heap.trace_object_ids(slot.current_record);
        }
        self.heap
            .with_payload_bytes(slot.current_record, |payload| {
                let mut out = Vec::new();
                object_heap::trace_object_refs_with_layout(layout, payload, &mut out)?;
                Ok(out)
            })
    }

    pub(crate) fn trace_persistent_publication_object_ids(
        &self,
        publication: &persist::PendingPublication,
    ) -> Result<Vec<ObjectId>> {
        let (domain, object_index) =
            crate::runtime::vm::unpack_object_granule_id(publication.logical_id)?;
        ensure!(
            publication.kind == domain as u16,
            "persistent publication kind does not match logical id domain"
        );
        let header = TxObjectHeader::read_from_prefix(&publication.payload)?;
        let kind = object_kind_from_u16(header.kind)?;
        let type_layout_id = TypeLayoutId::new(header.type_layout_id)
            .context("persistent publication object record type layout id cannot be zero")?;
        ensure!(
            header.object_id == object_index,
            "persistent publication object record id does not match logical id"
        );
        ensure!(
            header.version == publication.version,
            "persistent publication object record version does not match publication"
        );
        ensure!(
            header.type_layout_id == publication.type_layout_id,
            "persistent publication object record type layout id does not match publication"
        );
        ensure!(
            match domain {
                crate::runtime::vm::PackedGranuleDomain::TStruct => {
                    header.kind == ObjectKind::Struct as u16
                }
                crate::runtime::vm::PackedGranuleDomain::TArray => {
                    header.kind == ObjectKind::Array as u16
                }
                _ => false,
            },
            "persistent publication object record kind does not match logical id domain"
        );
        self.validate_type_layout_for_object_kind(kind, type_layout_id)?;
        self.trace_persistent_object_record_bytes(header, type_layout_id, &publication.payload)
    }

    fn trace_persistent_object_record_bytes(
        &self,
        header: TxObjectHeader,
        type_layout_id: TypeLayoutId,
        record_bytes: &[u8],
    ) -> Result<Vec<ObjectId>> {
        let array_length = if header.kind == ObjectKind::Array as u16 {
            ensure!(
                record_bytes.len() >= core::mem::size_of::<object_heap::TxArrayHeader>(),
                "serialized array record is shorter than expected"
            );
            Some(u32::from_le_bytes(
                record_bytes[core::mem::size_of::<TxObjectHeader>()
                    ..core::mem::size_of::<TxObjectHeader>() + core::mem::size_of::<u32>()]
                    .try_into()
                    .unwrap(),
            ))
        } else {
            None
        };
        let payload_start = object_heap::payload_offset_for_recovery(array_length);
        let payload = record_bytes
            .get(payload_start..)
            .context("persistent object record payload is out of bounds")?;
        let mut out = Vec::new();
        if uses_placeholder_persistent_trace_fallback(type_layout_id) {
            let decoded =
                object_heap::decode_payload_bytes_for_recovery(header, array_length, record_bytes)?;
            match decoded {
                ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => {
                    out.extend(fields.into_iter().filter_map(|value| match value {
                        ObjectValue::Ref(Some(object_id)) => Some(object_id),
                        _ => None,
                    }));
                }
            }
        } else {
            let layout = self.require_type_layout(type_layout_id)?;
            object_heap::trace_object_refs_with_layout(layout, payload, &mut out)?;
        }
        Ok(out)
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
        let type_layout_id = self.live_slot(object_id)?.type_layout_id;
        let runtime_type_index = self.live_slot(object_id)?.runtime_type_index;
        let persistent = self.live_slot(object_id)?.persistent;
        let record_version = self.bump_record_version()?;
        let record = self.heap.allocate_record(
            object_id,
            record_version,
            kind,
            0,
            type_layout_id,
            &payload,
        )?;
        let version = self.bump_object_version()?;
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_layout_id,
            runtime_type_index,
            persistent,
            current_record: record,
        });
        Ok(())
    }

    pub(crate) fn granule_id(&self, object_id: ObjectId) -> Result<GranuleId> {
        match self.kind(object_id)? {
            ObjectKind::Struct | ObjectKind::Array => Ok(GranuleId::Object { object_id }),
            ObjectKind::I31 | ObjectKind::Extern | ObjectKind::Func => {
                bail!(
                    "object kind is encoded as an inline durable value, not a transactional granule"
                )
            }
        }
    }

    pub(crate) fn object_pending_publication(
        &self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        let handle = self.live_slot(object_id)?.current_record;
        let payload = self.heap.payload(handle)?;
        self.validate_persistent_payload_refs(&payload)?;
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
            header.type_layout_id,
            payload,
        )
    }

    pub(crate) fn persistent_object_pending_publication_from_payload(
        &mut self,
        object_id: ObjectId,
        payload: &ObjectPayload,
    ) -> Result<persist::PendingPublication> {
        let record_version = self.bump_record_version()?;
        self.persistent_object_pending_publication_from_payload_with_record_version(
            object_id,
            payload,
            record_version,
        )
    }

    pub(crate) fn persistent_object_pending_publication_from_payload_with_record_version(
        &mut self,
        object_id: ObjectId,
        payload: &ObjectPayload,
        record_version: u32,
    ) -> Result<persist::PendingPublication> {
        let slot = self.live_slot(object_id)?.clone();
        ensure!(
            slot.persistent,
            "persistent object publication requires a persistent object: {object_id:?}"
        );
        ensure!(
            slot.kind == payload.kind(),
            "object payload kind does not match object table slot kind"
        );
        self.validate_persistent_payload_refs(payload)?;
        self.next_record_version = self.next_record_version.max(record_version);
        let record = object_heap::encode_object_record(
            object_id.object_index,
            record_version,
            slot.kind as u16,
            slot.type_layout_id,
            payload,
        )?;
        let domain = match slot.kind {
            ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
            ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
            other => bail!("object kind {other:?} is not a persistent object granule"),
        };
        persist::PendingPublication::persistent_object(
            domain,
            object_id.object_index,
            record_version,
            slot.type_layout_id,
            record,
        )
    }

    pub(crate) fn persistent_gc_copy_publication(
        &mut self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        let record_version = self.bump_record_version()?;
        self.persistent_gc_copy_publication_with_record_version(object_id, record_version)
    }

    pub(crate) fn persistent_gc_copy_publication_with_record_version(
        &mut self,
        object_id: ObjectId,
        record_version: u32,
    ) -> Result<persist::PendingPublication> {
        let slot = self.live_slot(object_id)?.clone();
        ensure!(
            slot.persistent,
            "persistent GC copy requires a persistent object: {object_id:?}"
        );
        let payload = self.heap.payload(slot.current_record)?;
        self.validate_persistent_payload_refs(&payload)?;
        self.next_record_version = self.next_record_version.max(record_version);
        let record = object_heap::encode_object_record(
            object_id.object_index,
            record_version,
            slot.kind as u16,
            slot.type_layout_id,
            &payload,
        )?;
        let domain = match slot.kind {
            ObjectKind::Struct => crate::runtime::vm::PackedGranuleDomain::TStruct,
            ObjectKind::Array => crate::runtime::vm::PackedGranuleDomain::TArray,
            other => bail!("object kind {other:?} is not a persistent object granule"),
        };
        persist::PendingPublication::persistent_object(
            domain,
            object_id.object_index,
            record_version,
            slot.type_layout_id,
            record,
        )
    }

    pub(crate) fn install_committed_mapped_persistent_publications(
        &mut self,
        publications: &[persist::PendingPublication],
        markers: &[persist::PendingCommitLogEntry],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<Vec<ObjectId>> {
        ensure!(
            publications.len() == markers.len(),
            "committed publication marker count does not match publication count"
        );
        let mut installed = Vec::new();
        for (publication, marker) in publications.iter().zip(markers) {
            if let Some(object_id) = self.install_committed_mapped_persistent_publication(
                publication,
                *marker,
                mapped_source.clone(),
            )? {
                installed.push(object_id);
            }
        }
        Ok(installed)
    }

    pub(crate) fn committed_mapped_persistent_publication_entries(
        &self,
        publications: &[persist::PendingPublication],
        markers: &[persist::PendingCommitLogEntry],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<Vec<InstalledPersistentObjectPublication>> {
        ensure!(
            publications.len() == markers.len(),
            "committed publication marker count does not match publication count"
        );
        let mut installed = Vec::new();
        for (publication, marker) in publications.iter().zip(markers) {
            if let Some(entry) = self.committed_mapped_persistent_publication_entry(
                publication,
                *marker,
                mapped_source.clone(),
            )? {
                installed.push(entry);
            }
        }
        Ok(installed)
    }

    pub(crate) fn install_persistent_object_directory_entry(
        &mut self,
        entry: PersistentObjectDirectoryEntry,
    ) -> Result<()> {
        let index = object_slot_index(entry.object_id)?;
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }
        let type_layout_id = TypeLayoutId::new(entry.type_layout_id)
            .context("persistent object directory entry type layout id cannot be zero")?;
        self.validate_type_layout_for_object_kind(entry.kind, type_layout_id)?;
        if let Some(slot) = self.slots[index].as_ref() {
            ensure!(
                slot.persistent,
                "persistent object directory entry collides with live volatile slot"
            );
        }

        let record = match &entry.record_source {
            Some(source) => self.heap.install_mapped_persistent_record_from_source(
                entry.object_id,
                entry.kind,
                entry.record_version,
                entry.type_layout_id,
                &source.mapped_source,
                &source.location,
            )?,
            None => {
                bail!("persistent object directory entry has no mapped record source")
            }
        };

        let was_live = self.slots[index].is_some();
        self.next_version = self.next_version.max(entry.directory_version);
        self.next_record_version = self.next_record_version.max(entry.record_version);
        self.slots[index] = Some(ObjectTableSlot {
            kind: entry.kind,
            version: entry.directory_version,
            type_layout_id: entry.type_layout_id,
            runtime_type_index: entry.runtime_type_index,
            persistent: true,
            current_record: record,
        });
        if !was_live {
            self.free_list.retain(|free| *free != entry.object_id);
            self.live_count = self
                .live_count
                .checked_add(1)
                .context("object table live count overflow")?;
        }
        Ok(())
    }

    pub(crate) fn validate_persistent_object_directory_entry_source(
        &self,
        entry: &PersistentObjectDirectoryEntry,
    ) -> Result<()> {
        let source = entry
            .record_source
            .as_ref()
            .context("persistent object directory entry has no mapped record source")?;
        object_heap::ObjectHeap::validate_mapped_persistent_record_from_source(
            entry.object_id,
            entry.kind,
            entry.record_version,
            entry.type_layout_id,
            &source.mapped_source,
            &source.location,
        )?;
        Ok(())
    }

    pub(crate) fn install_committed_mapped_persistent_publication(
        &mut self,
        publication: &persist::PendingPublication,
        marker: persist::PendingCommitLogEntry,
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<Option<ObjectId>> {
        let Some(mut installed) =
            self.committed_mapped_persistent_publication_entry(publication, marker, mapped_source)?
        else {
            return Ok(None);
        };
        installed.directory_entry.directory_version = self.bump_object_version()?;
        self.install_persistent_object_directory_entry(installed.directory_entry)?;
        Ok(Some(installed.object_id))
    }

    fn committed_mapped_persistent_publication_entry(
        &self,
        publication: &persist::PendingPublication,
        marker: persist::PendingCommitLogEntry,
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<Option<InstalledPersistentObjectPublication>> {
        let Some(type_layout_id) = publication.persistent_object_type_layout_id()? else {
            return Ok(None);
        };
        ensure!(
            marker.role == crate::runtime::vm::TxLogEntryRole::TObjectPub,
            "committed persistent object marker must reference an object publication"
        );
        ensure!(
            marker.logical_id == publication.logical_id && marker.version == publication.version,
            "committed persistent object marker does not match publication"
        );
        let (domain, object_index) = crate::runtime::vm::unpack_object_granule_id(
            publication.logical_id,
        )
        .context("committed persistent object publication logical id is not an object granule")?;
        ensure!(
            publication.kind == domain as u16,
            "committed persistent object publication kind does not match logical id domain"
        );
        let object_id = ObjectId { object_index };
        let slot = self.live_slot(object_id)?.clone();
        ensure!(
            slot.persistent,
            "committed persistent object install requires a persistent object: {object_id:?}"
        );

        let object_header = TxObjectHeader::read_from_prefix(&publication.payload)?;
        let kind = object_kind_from_u16(object_header.kind)?;
        let expected_kind = match domain {
            crate::runtime::vm::PackedGranuleDomain::TStruct => ObjectKind::Struct,
            crate::runtime::vm::PackedGranuleDomain::TArray => ObjectKind::Array,
            _ => unreachable!("unpack_object_granule_id only returns durable object domains"),
        };
        ensure!(
            kind == expected_kind,
            "committed persistent object record kind does not match publication domain"
        );
        ensure!(
            slot.kind == kind,
            "committed persistent object record kind does not match object slot kind"
        );
        ensure!(
            object_header.object_id == object_id.object_index,
            "committed persistent object record id does not match publication"
        );
        ensure!(
            object_header.version == publication.version,
            "committed persistent object record version does not match publication"
        );
        ensure!(
            object_header.type_layout_id == publication.type_layout_id,
            "committed persistent object record type layout id does not match publication"
        );
        ensure!(
            object_header.type_layout_id == type_layout_id.get(),
            "committed persistent object record type layout id is not registered"
        );
        let publication_payload_len = u64::try_from(publication.payload.len())
            .context("committed persistent object payload length overflow")?;
        ensure!(
            publication_payload_len == object_header.record_len,
            "committed persistent object publication payload length does not match object record length"
        );
        self.validate_type_layout_for_object_kind(kind, type_layout_id)?;

        let data_record_offset = durable_data_record_offset(marker.data_block, marker.data_offset)?;
        let location = PersistentObjectRecordLocation {
            data_block: marker.data_block,
            data_offset: marker.data_offset,
            data_record_offset: u64::try_from(data_record_offset)
                .context("committed persistent object data record offset overflow")?,
            record_len: object_header.record_len,
        };
        Ok(Some(InstalledPersistentObjectPublication {
            object_id,
            directory_entry: PersistentObjectDirectoryEntry {
                object_id,
                kind,
                directory_version: 0,
                record_version: object_header.version,
                type_layout_id: object_header.type_layout_id,
                runtime_type_index: slot.runtime_type_index,
                record_source: Some(PersistentObjectRecordSource {
                    mapped_source,
                    location,
                }),
            },
        }))
    }

    pub(crate) fn install_committed_serialized_persistent_publications(
        &mut self,
        publications: &[persist::PendingPublication],
    ) -> Result<Vec<ObjectId>> {
        let mut installed = Vec::new();
        for publication in publications {
            if publication.persistent_object_type_layout_id()?.is_none() {
                continue;
            }
            installed.push(self.install_persistent_gc_copied_publication(publication)?);
        }
        Ok(installed)
    }

    pub(crate) fn install_persistent_gc_copied_publication(
        &mut self,
        publication: &persist::PendingPublication,
    ) -> Result<ObjectId> {
        let (domain, object_index) =
            crate::runtime::vm::unpack_object_granule_id(publication.logical_id)
                .context("persistent GC copy publication logical id is not an object granule")?;
        ensure!(
            matches!(
                domain,
                crate::runtime::vm::PackedGranuleDomain::TStruct
                    | crate::runtime::vm::PackedGranuleDomain::TArray
            ),
            "persistent GC copy publication domain must be a durable object domain"
        );
        let object_id = ObjectId { object_index };
        let index = object_slot_index(object_id)?;
        let slot = self.live_slot(object_id)?.clone();
        ensure!(
            slot.persistent,
            "persistent GC copy install requires a persistent object: {object_id:?}"
        );

        let record = self.heap.install_record_bytes(&publication.payload)?;
        let header = self.heap.header(record)?;
        let kind = object_kind_from_u16(header.kind)?;
        let expected_kind = match domain {
            crate::runtime::vm::PackedGranuleDomain::TStruct => ObjectKind::Struct,
            crate::runtime::vm::PackedGranuleDomain::TArray => ObjectKind::Array,
            _ => unreachable!("domain was checked above"),
        };
        ensure!(
            header.object_id == object_id.object_index,
            "persistent GC copied record id does not match publication"
        );
        ensure!(
            header.version == publication.version,
            "persistent GC copied record version does not match publication"
        );
        ensure!(
            kind == expected_kind,
            "persistent GC copied record kind does not match publication domain"
        );
        ensure!(
            header.type_layout_id == publication.type_layout_id,
            "persistent GC copied record type layout id does not match publication"
        );
        ensure!(
            slot.kind == kind,
            "persistent GC copied record kind does not match object slot kind"
        );
        self.validate_type_layout_for_object_kind(
            kind,
            TypeLayoutId::new(header.type_layout_id)
                .context("persistent GC copied record type layout id cannot be zero")?,
        )?;
        self.next_record_version = self.next_record_version.max(header.version);
        let version = self.bump_object_version()?;
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_layout_id: header.type_layout_id,
            runtime_type_index: slot.runtime_type_index,
            persistent: true,
            current_record: record,
        });
        Ok(object_id)
    }

    #[cfg(test)]
    pub(crate) fn persistent_gc_copy_publication_for_test(
        &mut self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        self.persistent_gc_copy_publication(object_id)
    }

    #[cfg(test)]
    pub(crate) fn install_persistent_gc_copied_publication_for_test(
        &mut self,
        publication: &persist::PendingPublication,
    ) -> Result<ObjectId> {
        self.install_persistent_gc_copied_publication(publication)
    }

    pub(crate) fn validate_persistent_payload_refs(&self, payload: &ObjectPayload) -> Result<()> {
        let values = match payload {
            ObjectPayload::Struct(fields) => fields.as_slice(),
            ObjectPayload::Array(elements) => elements.as_slice(),
        };
        for value in values {
            let ObjectValue::Ref(Some(object_id)) = value else {
                continue;
            };
            let Ok(slot) = self.live_slot(*object_id) else {
                bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
            };
            ensure!(slot.persistent, VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
        }
        Ok(())
    }

    pub(crate) fn free(&mut self, object_id: ObjectId) -> Result<bool> {
        let shared_persistent = self.shared_region_runtime.is_some()
            && self
                .live_slot(object_id)
                .map(|slot| slot.persistent)
                .unwrap_or(false);
        if !self.clear_live_slot_for_volatile_gc(object_id)? {
            return Ok(false);
        }
        self.bump_object_version()?;
        if !shared_persistent {
            self.free_list.push(object_id);
        }
        Ok(true)
    }

    pub(crate) fn live_slot(&self, object_id: ObjectId) -> Result<&ObjectTableSlot> {
        let index = object_slot_index(object_id)?;
        self.slots
            .get(index)
            .and_then(Option::as_ref)
            .with_context(|| format!("object table slot is not live: {object_id:?}"))
    }

    pub(crate) fn clear_live_slot_for_volatile_gc(&mut self, object_id: ObjectId) -> Result<bool> {
        let index = object_slot_index(object_id)?;
        if index >= self.slots.len() {
            return Ok(false);
        }
        if self.slots[index].is_none() {
            return Ok(false);
        }
        self.slots[index] = None;
        if let Some(handle) = self.objects_to_transaction_ref_handles.remove(&object_id) {
            self.transaction_ref_handles_to_objects.remove(&handle);
        }
        if let Some(gc_ref) = self.object_to_live_bridge_gc_ref.remove(&object_id) {
            self.live_bridge_gc_refs_to_objects.remove(&gc_ref);
        }
        self.live_count = self
            .live_count
            .checked_sub(1)
            .context("object table live count underflow")?;
        Ok(true)
    }

    pub(crate) fn bump_object_version(&mut self) -> Result<u64> {
        self.next_version = self
            .next_version
            .checked_add(1)
            .context("object table version overflow")?;
        Ok(self.next_version)
    }

    pub(crate) fn bump_record_version(&mut self) -> Result<u32> {
        self.next_record_version = self
            .next_record_version
            .checked_add(1)
            .context("object record version overflow")?;
        Ok(self.next_record_version)
    }

    pub(crate) fn clear_volatile_index(&mut self) {
        self.slots.clear();
        self.free_list.clear();
        self.transaction_ref_handles_to_objects.clear();
        self.objects_to_transaction_ref_handles.clear();
        self.next_transaction_ref_handle = FIRST_TRANSACTION_OBJECT_REF_HANDLE;
        self.live_bridge_gc_refs_to_objects.clear();
        self.object_to_live_bridge_gc_ref.clear();
        self.next_version = 0;
        self.next_record_version = 0;
        self.live_count = 0;
    }

    pub(crate) fn rebuild_free_list_holes(&mut self) -> Result<()> {
        self.free_list.clear();
        if self.shared_region_runtime.is_some() {
            return Ok(());
        }
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.is_none() {
                self.free_list.push(ObjectId {
                    object_index: u64::try_from(index).context("slot index does not fit u64")?,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn rebuild_volatile_index_from_heap(&mut self) -> Result<()> {
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
                type_layout_id: header.type_layout_id,
                runtime_type_index: None,
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

    fn rebuild_slots_for_recovered_winners(
        &mut self,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
    ) -> Result<()> {
        if let Some(slot_len) = rebuild_recovered_slots_len(winners)? {
            self.slots.resize(slot_len, None);
        }
        Ok(())
    }

    fn install_recovered_slot_from_handle(
        &mut self,
        winner: &crate::runtime::vm::RecoveredObjectWinner,
        handle: object_heap::TxRecordHandle,
    ) -> Result<()> {
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

        let header = self.heap.header(handle)?;
        let kind = object_kind_from_u16(header.kind)?;
        let type_layout_id = TypeLayoutId::new(header.type_layout_id)
            .context("recovered object record type layout id cannot be zero")?;

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
            header.type_layout_id == winner.type_layout_id,
            "recovered object record type layout id does not match winner"
        );
        self.validate_type_layout_for_object_kind(kind, type_layout_id)?;

        self.next_record_version = self.next_record_version.max(header.version);
        let version = self.bump_object_version()?;
        self.slots[index] = Some(ObjectTableSlot {
            kind,
            version,
            type_layout_id: header.type_layout_id,
            runtime_type_index: None,
            persistent: true,
            current_record: handle,
        });
        self.live_count = self
            .live_count
            .checked_add(1)
            .context("object table live count overflow during recovery rebuild")?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn rebuild_from_recovered_object_winners(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        source: &dyn MappedRegionSource,
    ) -> Result<()> {
        self.clear_volatile_index();
        self.heap = object_heap::ObjectHeap::default();
        self.install_recovered_type_layouts(recovered_type_layouts)?;
        self.rebuild_slots_for_recovered_winners(winners)?;

        for winner in winners {
            let record_bytes = copied_recovered_winner_record_bytes(winner, source)?;
            let handle = self.heap.install_record_bytes(&record_bytes)?;
            self.install_recovered_slot_from_handle(winner, handle)?;
        }

        self.rebuild_free_list_holes()?;
        Ok(())
    }

    pub(crate) fn rebuild_from_mapped_recovered_object_winners(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<()> {
        self.clear_volatile_index();
        self.heap = object_heap::ObjectHeap::default();
        self.install_recovered_type_layouts(recovered_type_layouts)?;
        self.rebuild_slots_for_recovered_winners(winners)?;

        for winner in winners {
            let mut handle = None;
            winner.with_source_record_bytes(&*mapped_source, &mut |record_bytes| {
                handle = Some(self.heap.install_mapped_persistent_record(
                    winner,
                    mapped_source.clone(),
                    record_bytes,
                )?);
                Ok(())
            })?;
            let handle = handle.context("mapped recovered object was not installed")?;
            self.install_recovered_slot_from_handle(winner, handle)?;
        }

        self.rebuild_free_list_holes()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn rebuild_reachable_from_recovered_object_winners(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        root_object_ids: &[u64],
        source: &dyn MappedRegionSource,
    ) -> Result<PersistentRecoveryGcReport> {
        let report = Self::persistent_recovery_gc_report(
            recovered_type_layouts,
            winners,
            root_object_ids,
            source,
        )?;
        let reachable_winners = winners
            .iter()
            .filter(|winner| {
                report.mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut rebuilt = ObjectTable::default();
        rebuilt.rebuild_from_recovered_object_winners(
            recovered_type_layouts,
            &reachable_winners,
            source,
        )?;
        *self = rebuilt;
        Ok(report)
    }

    pub(crate) fn rebuild_reachable_from_mapped_recovered_object_winners(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        root_object_ids: &[u64],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<PersistentRecoveryGcReport> {
        let report = Self::persistent_recovery_gc_report_mapped(
            recovered_type_layouts,
            winners,
            root_object_ids,
            mapped_source.clone(),
        )?;
        let reachable_winners = winners
            .iter()
            .filter(|winner| {
                report.mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut rebuilt = ObjectTable::default();
        rebuilt.rebuild_from_mapped_recovered_object_winners(
            recovered_type_layouts,
            &reachable_winners,
            mapped_source,
        )?;
        *self = rebuilt;
        Ok(report)
    }

    #[cfg(test)]
    pub(crate) fn persistent_recovery_gc_report(
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        root_object_ids: &[u64],
        source: &dyn MappedRegionSource,
    ) -> Result<PersistentRecoveryGcReport> {
        let winner_map = winners.iter().try_fold(
            BTreeMap::<u64, &crate::runtime::vm::RecoveredObjectWinner>::new(),
            |mut winners_by_id, winner| {
                ensure!(
                    winners_by_id.insert(winner.object_id, winner).is_none(),
                    "duplicate recovered object winner for object id {}",
                    winner.object_id
                );
                Ok(winners_by_id)
            },
        )?;
        let mut layout_table = ObjectTable::default();
        layout_table.install_recovered_type_layouts(recovered_type_layouts)?;

        let mut reachable = BTreeSet::new();
        let mut grey = Vec::new();
        let mut dangling_refs = Vec::new();
        let mut invalid_roots = Vec::new();

        for &object_index in root_object_ids {
            let root = ObjectId { object_index };
            if winner_map.contains_key(&object_index) {
                if reachable.insert(root) {
                    grey.push(root);
                }
            } else {
                invalid_roots.push(PersistentRootError {
                    root,
                    kind: PersistentRootErrorKind::Missing,
                });
            }
        }

        while let Some(object) = grey.pop() {
            let winner = winner_map
                .get(&object.object_index)
                .copied()
                .context("reachable recovered object winner disappeared")?;
            for child in Self::trace_recovered_winner_object_ids(&layout_table, winner, source)? {
                if winner_map.contains_key(&child.object_index) {
                    if reachable.insert(child) {
                        grey.push(child);
                    }
                } else {
                    dangling_refs.push(DanglingObjectRef {
                        from: object,
                        to: child,
                        kind: DanglingObjectRefKind::Missing,
                    });
                }
            }
        }

        let unreachable_persistent = winner_map
            .keys()
            .map(|&object_index| ObjectId { object_index })
            .filter(|object_id| !reachable.contains(object_id))
            .collect::<BTreeSet<_>>();
        let mark = PersistentObjectMarkReport {
            reachable,
            unreachable_persistent,
            dangling_refs,
            invalid_roots,
        };
        let installed_winners = winners
            .iter()
            .filter(|winner| {
                mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(|winner| winner.object_id)
            .collect::<Vec<_>>();
        let skipped_unreachable_winners = winners
            .iter()
            .filter(|winner| {
                mark.unreachable_persistent.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(|winner| winner.object_id)
            .collect::<Vec<_>>();
        let reachable_record_locations = winners
            .iter()
            .filter(|winner| {
                mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(recovered_record_location_from_winner)
            .collect::<Vec<_>>();
        let unreachable_record_locations = winners
            .iter()
            .filter(|winner| {
                mark.unreachable_persistent.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(recovered_record_location_from_winner)
            .collect::<Vec<_>>();
        Ok(PersistentRecoveryGcReport {
            mark,
            installed_winners,
            skipped_unreachable_winners,
            reachable_record_locations,
            unreachable_record_locations,
        })
    }

    pub(crate) fn persistent_recovery_gc_report_mapped(
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        root_object_ids: &[u64],
        mapped_source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<PersistentRecoveryGcReport> {
        let winner_map = winners.iter().try_fold(
            BTreeMap::<u64, &crate::runtime::vm::RecoveredObjectWinner>::new(),
            |mut winners_by_id, winner| {
                ensure!(
                    winners_by_id.insert(winner.object_id, winner).is_none(),
                    "duplicate recovered object winner for object id {}",
                    winner.object_id
                );
                Ok(winners_by_id)
            },
        )?;
        let mut layout_table = ObjectTable::default();
        layout_table.install_recovered_type_layouts(recovered_type_layouts)?;

        let mut reachable = BTreeSet::new();
        let mut grey = Vec::new();
        let mut dangling_refs = Vec::new();
        let mut invalid_roots = Vec::new();

        for &object_index in root_object_ids {
            let root = ObjectId { object_index };
            if winner_map.contains_key(&object_index) {
                if reachable.insert(root) {
                    grey.push(root);
                }
            } else {
                invalid_roots.push(PersistentRootError {
                    root,
                    kind: PersistentRootErrorKind::Missing,
                });
            }
        }

        while let Some(object) = grey.pop() {
            let winner = winner_map
                .get(&object.object_index)
                .copied()
                .context("reachable recovered object winner disappeared")?;
            for child in Self::trace_recovered_winner_object_ids_mapped(
                &layout_table,
                winner,
                &*mapped_source,
            )? {
                if winner_map.contains_key(&child.object_index) {
                    if reachable.insert(child) {
                        grey.push(child);
                    }
                } else {
                    dangling_refs.push(DanglingObjectRef {
                        from: object,
                        to: child,
                        kind: DanglingObjectRefKind::Missing,
                    });
                }
            }
        }

        let unreachable_persistent = winner_map
            .keys()
            .map(|&object_index| ObjectId { object_index })
            .filter(|object_id| !reachable.contains(object_id))
            .collect::<BTreeSet<_>>();
        let mark = PersistentObjectMarkReport {
            reachable,
            unreachable_persistent,
            dangling_refs,
            invalid_roots,
        };
        let installed_winners = winners
            .iter()
            .filter(|winner| {
                mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(|winner| winner.object_id)
            .collect::<Vec<_>>();
        let skipped_unreachable_winners = winners
            .iter()
            .filter(|winner| {
                mark.unreachable_persistent.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(|winner| winner.object_id)
            .collect::<Vec<_>>();
        let reachable_record_locations = winners
            .iter()
            .filter(|winner| {
                mark.reachable.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(recovered_record_location_from_winner)
            .collect::<Vec<_>>();
        let unreachable_record_locations = winners
            .iter()
            .filter(|winner| {
                mark.unreachable_persistent.contains(&ObjectId {
                    object_index: winner.object_id,
                })
            })
            .map(recovered_record_location_from_winner)
            .collect::<Vec<_>>();
        Ok(PersistentRecoveryGcReport {
            mark,
            installed_winners,
            skipped_unreachable_winners,
            reachable_record_locations,
            unreachable_record_locations,
        })
    }

    #[cfg(test)]
    pub(crate) fn trace_recovered_winner_object_ids(
        layouts: &ObjectTable,
        winner: &crate::runtime::vm::RecoveredObjectWinner,
        source: &dyn MappedRegionSource,
    ) -> Result<Vec<ObjectId>> {
        let record_bytes = copied_recovered_winner_record_bytes(winner, source)?;
        let header = TxObjectHeader::read_from_prefix(&record_bytes)?;
        let kind = object_kind_from_u16(header.kind)?;
        let type_layout_id = TypeLayoutId::new(header.type_layout_id)
            .context("recovered object record type layout id cannot be zero")?;

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
            header.type_layout_id == winner.type_layout_id,
            "recovered object record type layout id does not match winner"
        );
        layouts.validate_type_layout_for_object_kind(kind, type_layout_id)?;

        let mut heap = object_heap::ObjectHeap::default();
        let handle = heap.install_record_bytes(&record_bytes)?;
        if uses_placeholder_persistent_trace_fallback(type_layout_id) {
            return heap.trace_object_ids(handle);
        }
        let layout = layouts.require_type_layout(type_layout_id)?;
        let payload = heap.payload_bytes(handle)?;
        let mut out = Vec::new();
        object_heap::trace_object_refs_with_layout(layout, &payload, &mut out)?;
        Ok(out)
    }

    fn trace_recovered_winner_object_ids_mapped(
        layouts: &ObjectTable,
        winner: &crate::runtime::vm::RecoveredObjectWinner,
        mapped_source: &dyn MappedRegionSource,
    ) -> Result<Vec<ObjectId>> {
        let mut traced = None;
        winner.with_source_record_bytes(mapped_source, &mut |record_bytes| {
            let header = TxObjectHeader::read_from_prefix(record_bytes)?;
            let kind = object_kind_from_u16(header.kind)?;
            let type_layout_id = TypeLayoutId::new(header.type_layout_id)
                .context("recovered object record type layout id cannot be zero")?;

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
                header.type_layout_id == winner.type_layout_id,
                "recovered object record type layout id does not match winner"
            );
            layouts.validate_type_layout_for_object_kind(kind, type_layout_id)?;

            let array_length = if header.kind == ObjectKind::Array as u16 {
                ensure!(
                    record_bytes.len() >= core::mem::size_of::<object_heap::TxArrayHeader>(),
                    "serialized array record is shorter than expected"
                );
                Some(u32::from_le_bytes(
                    record_bytes[core::mem::size_of::<TxObjectHeader>()
                        ..core::mem::size_of::<TxObjectHeader>() + core::mem::size_of::<u32>()]
                        .try_into()
                        .unwrap(),
                ))
            } else {
                None
            };
            let payload_start = object_heap::payload_offset_for_recovery(array_length);
            let payload = record_bytes
                .get(payload_start..)
                .context("mapped recovered object payload is out of bounds")?;
            let mut out = Vec::new();
            if uses_placeholder_persistent_trace_fallback(type_layout_id) {
                let decoded = object_heap::decode_payload_bytes_for_recovery(
                    header,
                    array_length,
                    record_bytes,
                )?;
                match decoded {
                    ObjectPayload::Struct(fields) | ObjectPayload::Array(fields) => {
                        out.extend(fields.into_iter().filter_map(|value| match value {
                            ObjectValue::Ref(Some(object_id)) => Some(object_id),
                            _ => None,
                        }));
                    }
                }
            } else {
                let layout = layouts.require_type_layout(type_layout_id)?;
                object_heap::trace_object_refs_with_layout(layout, payload, &mut out)?;
            }
            traced = Some(out);
            Ok(())
        })?;
        traced.context("mapped recovered object was not traced")
    }

    #[cfg(test)]
    pub(crate) fn current_record_handle_for_test(
        &self,
        object_id: ObjectId,
    ) -> Result<object_heap::TxRecordHandle> {
        Ok(self.live_slot(object_id)?.current_record)
    }

    #[cfg(test)]
    pub(crate) fn pending_publication_for_test(
        &self,
        object_id: ObjectId,
    ) -> Result<persist::PendingPublication> {
        self.object_pending_publication(object_id)
    }

    #[cfg(test)]
    pub(crate) fn rebuild_from_recovery_with_source_for_test(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<()> {
        self.rebuild_from_mapped_recovered_object_winners(recovered_type_layouts, winners, source)
    }

    #[cfg(test)]
    pub(crate) fn rebuild_reachable_from_recovery_with_source_for_test(
        &mut self,
        recovered_type_layouts: &TypeLayoutRegistry,
        winners: &[crate::runtime::vm::RecoveredObjectWinner],
        root_object_ids: &[u64],
        source: Arc<dyn crate::runtime::vm::block_region::MappedRegionSource>,
    ) -> Result<PersistentRecoveryGcReport> {
        self.rebuild_reachable_from_mapped_recovered_object_winners(
            recovered_type_layouts,
            winners,
            root_object_ids,
            source,
        )
    }

    #[cfg(test)]
    pub(crate) fn allocate_payload_with_type_layout_id_for_test(
        &mut self,
        payload: ObjectPayload,
        type_layout_id: TypeLayoutId,
    ) -> Result<ObjectId> {
        self.allocate_payload_with_type_layout_id(payload, type_layout_id, false)
    }

    pub(crate) fn validate_type_layout_for_object_kind(
        &self,
        kind: ObjectKind,
        type_layout_id: TypeLayoutId,
    ) -> Result<()> {
        let layout = self.require_type_layout(type_layout_id)?;
        ensure!(
            persistent_type_kind_matches_object_kind(layout.kind(), kind),
            "persistent type layout kind {:?} does not match object kind {:?}",
            layout.kind(),
            kind
        );
        Ok(())
    }

    pub(crate) fn persistent_object_ids(&self) -> Result<BTreeSet<ObjectId>> {
        let mut objects = BTreeSet::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.as_ref().is_some_and(|slot| slot.persistent) {
                objects.insert(ObjectId {
                    object_index: u64::try_from(index)
                        .context("object table slot index does not fit u64")?,
                });
            }
        }
        Ok(objects)
    }

    pub(crate) fn apply_volatile_persistent_sweep(
        &mut self,
        mark: &PersistentObjectMarkReport,
    ) -> Result<PersistentVolatileSweepReport> {
        mark.ensure_sweepable()?;
        let mut report = PersistentVolatileSweepReport::default();
        for object_id in self.persistent_object_ids()? {
            if mark.reachable.contains(&object_id) {
                report.retained_objects.push(object_id);
            } else {
                self.clear_live_slot_for_volatile_gc(object_id)?;
                report.removed_objects.push(object_id);
            }
        }
        self.rebuild_free_list_holes()?;
        Ok(report)
    }

    pub(crate) fn persistent_mark_sweep_from_roots<I>(
        &mut self,
        roots: I,
    ) -> Result<PersistentMarkSweepReport>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        let mark = PersistentObjectMarker::mark(self, roots)?;
        let sweep = self.apply_volatile_persistent_sweep(&mark)?;
        Ok(PersistentMarkSweepReport { mark, sweep })
    }

    #[cfg(test)]
    pub(crate) fn persistent_mark_sweep_from_roots_for_test<I>(
        &mut self,
        roots: I,
    ) -> Result<PersistentMarkSweepReport>
    where
        I: IntoIterator<Item = ObjectId>,
    {
        self.persistent_mark_sweep_from_roots(roots)
    }

    #[cfg(test)]
    pub(crate) fn current_record_is_persistent_mapped_for_test(
        &self,
        object_id: ObjectId,
    ) -> Result<bool> {
        let handle = self.live_slot(object_id)?.current_record;
        self.heap.is_persistent_mapped_for_test(handle)
    }
}

pub(super) fn recovered_record_location_from_winner(
    winner: &crate::runtime::vm::RecoveredObjectWinner,
) -> object_gc::PersistentRecoveredRecordLocation {
    let durable_record_len = winner
        .durable_data_record_len()
        .expect("persistent recovered record length overflow");
    object_gc::PersistentRecoveredRecordLocation {
        object_id: ObjectId {
            object_index: winner.object_id,
        },
        version: winner.version,
        data_block: winner.data_block,
        data_offset: winner.data_offset,
        record_len: durable_record_len,
    }
}

#[cfg(test)]
fn copied_recovered_winner_record_bytes(
    winner: &crate::runtime::vm::RecoveredObjectWinner,
    source: &dyn MappedRegionSource,
) -> Result<Vec<u8>> {
    let mut record_bytes = Vec::new();
    winner.with_source_record_bytes(source, &mut |bytes| {
        record_bytes.extend_from_slice(bytes);
        Ok(())
    })?;
    Ok(record_bytes)
}

fn rebuild_recovered_slots_len(
    winners: &[crate::runtime::vm::RecoveredObjectWinner],
) -> Result<Option<usize>> {
    winners
        .iter()
        .map(|winner| winner.object_id)
        .max()
        .map(|max_object_id| {
            usize::try_from(
                max_object_id
                    .checked_add(1)
                    .context("object slot range overflow")?,
            )
            .context("object slot range does not fit usize")
        })
        .transpose()
}

fn durable_data_record_offset(data_block: u32, data_offset: u32) -> Result<usize> {
    let block = usize::try_from(data_block).context("durable data block overflow")?;
    let offset = usize::try_from(data_offset).context("durable data offset overflow")?;
    ensure!(
        offset < BLOCK_SIZE,
        "durable data offset exceeds block size"
    );
    block
        .checked_mul(BLOCK_SIZE)
        .and_then(|base| base.checked_add(offset))
        .context("durable data record offset overflow")
}

/// Retires whole-dead object-data chunks in a recovered file-backed region image.
pub fn retire_unreachable_object_chunks_for_test(
    path: &Path,
    unreachable_object_ids: &[u64],
) -> Result<Vec<u32>> {
    let recovered =
        crate::runtime::vm::block_region::reopen_and_recover_file_backed_region_for_runtime(path)?;
    let unreachable_object_ids = unreachable_object_ids
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut reachable = Vec::new();
    let mut unreachable = Vec::new();

    for winner in recovered.committed_object_winners()? {
        let location = recovered_record_location_from_winner(&winner);
        if unreachable_object_ids.contains(&winner.object_id) {
            unreachable.push(location);
        } else {
            reachable.push(location);
        }
    }

    let mut region =
        crate::runtime::vm::block_region::FileBackedMemoryBlockRegion::open_for_test(path)?;
    let retired = region.retire_whole_dead_object_chunks(&reachable, &unreachable)?;
    region.fence()?;
    Ok(retired)
}

fn default_type_layout_id_for_kind(kind: ObjectKind) -> TypeLayoutId {
    match kind {
        ObjectKind::Struct => TypeLayoutId::DEFAULT_STRUCT,
        ObjectKind::Array => TypeLayoutId::DEFAULT_ARRAY,
        ObjectKind::I31 => TypeLayoutId::BUILTIN_I31,
        ObjectKind::Extern => TypeLayoutId::BUILTIN_EXTERN,
        ObjectKind::Func => TypeLayoutId::BUILTIN_FUNC,
    }
}

fn persistent_type_kind_matches_object_kind(
    type_kind: PersistentTypeKind,
    object_kind: ObjectKind,
) -> bool {
    matches!(
        (type_kind, object_kind),
        (PersistentTypeKind::Struct, ObjectKind::Struct)
            | (PersistentTypeKind::Array, ObjectKind::Array)
            | (PersistentTypeKind::I31, ObjectKind::I31)
            | (PersistentTypeKind::Extern, ObjectKind::Extern)
            | (PersistentTypeKind::Func, ObjectKind::Func)
    )
}

fn uses_placeholder_persistent_trace_fallback(type_layout_id: TypeLayoutId) -> bool {
    matches!(
        type_layout_id,
        TypeLayoutId::DEFAULT_STRUCT | TypeLayoutId::DEFAULT_ARRAY
    )
}

fn is_builtin_type_layout_id(type_layout_id: TypeLayoutId) -> bool {
    matches!(
        type_layout_id,
        TypeLayoutId::DEFAULT_STRUCT
            | TypeLayoutId::DEFAULT_ARRAY
            | TypeLayoutId::BUILTIN_I31
            | TypeLayoutId::BUILTIN_EXTERN
            | TypeLayoutId::BUILTIN_FUNC
    )
}

fn builtin_type_layouts() -> [PersistentTypeLayout; 5] {
    [
        PersistentTypeLayout::Struct {
            id: TypeLayoutId::DEFAULT_STRUCT,
            fingerprint: 0x5354_5255_4354_0001,
            body_size: 0,
            fields: Vec::new(),
        },
        PersistentTypeLayout::Array {
            id: TypeLayoutId::DEFAULT_ARRAY,
            fingerprint: 0x4152_5241_5900_0001,
            element_size: 0,
            element_kind: TraceSlotKind::Scalar,
        },
        PersistentTypeLayout::Scalar {
            id: TypeLayoutId::BUILTIN_I31,
            fingerprint: 0x4933_3100_0000_0001,
            kind: PersistentTypeKind::I31,
        },
        PersistentTypeLayout::Scalar {
            id: TypeLayoutId::BUILTIN_EXTERN,
            fingerprint: 0x4558_5445_524e_0001,
            kind: PersistentTypeKind::Extern,
        },
        PersistentTypeLayout::Scalar {
            id: TypeLayoutId::BUILTIN_FUNC,
            fingerprint: 0x4655_4e43_0000_0001,
            kind: PersistentTypeKind::Func,
        },
    ]
}

fn object_slot_index(object_id: ObjectId) -> Result<usize> {
    usize::try_from(object_id.object_index).context("object id does not fit host usize")
}
