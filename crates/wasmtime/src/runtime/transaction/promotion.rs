use super::*;

struct PromotionAttempt {
    initial_allocated_object_count: usize,
    promoted_sources: Vec<ObjectId>,
    promoted_objects: Vec<ObjectId>,
    promoted_gc_refs: Vec<u32>,
    durable_leaf_gc_refs: Vec<u32>,
}

impl PromotionAttempt {
    fn new(state: &TransactionState) -> Self {
        Self {
            initial_allocated_object_count: state.allocated_objects.len(),
            promoted_sources: Vec::new(),
            promoted_objects: Vec::new(),
            promoted_gc_refs: Vec::new(),
            durable_leaf_gc_refs: Vec::new(),
        }
    }

    fn record_promoted_object(&mut self, source: ObjectId, promoted: ObjectId) {
        self.promoted_sources.push(source);
        self.promoted_objects.push(promoted);
    }

    fn record_promoted_gc_ref(&mut self, gc_ref: u32, promoted: ObjectId) {
        self.promoted_gc_refs.push(gc_ref);
        self.promoted_objects.push(promoted);
    }

    fn record_durable_leaf_gc_ref(&mut self, gc_ref: u32) {
        self.durable_leaf_gc_refs.push(gc_ref);
    }

    fn rollback(self, state: &mut TransactionState, object_table: &mut ObjectTable) -> Result<()> {
        for gc_ref in self.durable_leaf_gc_refs {
            state.durable_leaf_gc_refs.remove(&gc_ref);
        }
        for gc_ref in self.promoted_gc_refs {
            state.promoted_gc_refs.remove(&gc_ref);
        }
        for source in self.promoted_sources {
            state.promoted_objects.remove(&source);
        }
        for promoted in &self.promoted_objects {
            state.staged_objects.remove(promoted);
        }
        state
            .allocated_objects
            .truncate(self.initial_allocated_object_count);
        for promoted in self.promoted_objects.into_iter().rev() {
            if object_table.live_slot(promoted).is_ok() {
                object_table.free(promoted)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OrdinaryGcPromotionValue {
    I31(i32),
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    V128([u8; 16]),
    GcRef(Option<u32>),
    FuncRef(DurableFuncIdentity),
    ExternRef(DurableExternIdentity),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OrdinaryGcPromotionSource {
    Struct {
        type_layout_id: TypeLayoutId,
        fields: Vec<OrdinaryGcPromotionValue>,
    },
    Array {
        type_layout_id: TypeLayoutId,
        elements: Vec<OrdinaryGcPromotionValue>,
    },
    I31(i32),
    FuncRef(DurableFuncIdentity),
    ExternRef(DurableExternIdentity),
    Unsupported(&'static str),
}

pub(crate) trait OrdinaryGcPromotionAdapter {
    fn promotion_source_for_gc_ref(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: u32,
    ) -> Result<Option<OrdinaryGcPromotionSource>>;
}

struct NoOrdinaryGcPromotionAdapter;

impl OrdinaryGcPromotionAdapter for NoOrdinaryGcPromotionAdapter {
    fn promotion_source_for_gc_ref(
        &mut self,
        _object_table: &mut ObjectTable,
        _gc_ref: u32,
    ) -> Result<Option<OrdinaryGcPromotionSource>> {
        Ok(None)
    }
}

impl TransactionState {
    pub(super) fn promote_transaction_object_graph(
        &mut self,
        object_table: &mut ObjectTable,
        source: ObjectId,
    ) -> Result<ObjectId> {
        self.run_promotion_attempt(object_table, |state, object_table, attempt| {
            state.promote_transaction_object_graph_in_attempt(object_table, source, attempt)
        })
    }

    fn rewrite_payload_refs_for_promotion(
        &mut self,
        object_table: &mut ObjectTable,
        payload: ObjectPayload,
    ) -> Result<ObjectPayload> {
        self.run_promotion_attempt(object_table, |state, object_table, attempt| {
            state.rewrite_payload_refs_for_promotion_in_attempt(object_table, payload, attempt)
        })
    }

    fn promote_transaction_object_graph_in_attempt(
        &mut self,
        object_table: &mut ObjectTable,
        source: ObjectId,
        attempt: &mut PromotionAttempt,
    ) -> Result<ObjectId> {
        self.ensure_active()?;
        if object_table.is_persistent(source)? {
            return Ok(source);
        }
        if let Some(promoted) = self.promoted_objects.get(&source).copied() {
            return Ok(promoted);
        }

        let kind = object_table.kind(source)?;
        ensure!(
            matches!(kind, ObjectKind::Struct | ObjectKind::Array),
            "transactional promotion currently supports struct and array object payloads"
        );
        if matches!(kind, ObjectKind::Struct | ObjectKind::Array) {
            self.acquire_object_read(object_table, source)?;
        }
        let type_layout_id = TypeLayoutId::new(object_table.live_slot(source)?.type_layout_id)
            .context("promoted object source layout id cannot be zero")?;
        let promoted =
            object_table.reserve_persistent_object_id_for_promotion(kind, type_layout_id)?;
        self.record_allocated_object(promoted)?;
        self.promoted_objects.insert(source, promoted);
        attempt.record_promoted_object(source, promoted);

        let source_payload = self
            .staged_objects
            .get(&source)
            .cloned()
            .unwrap_or(object_table.payload(source)?);
        let promoted_payload = self.rewrite_payload_refs_for_promotion_in_attempt(
            object_table,
            source_payload,
            attempt,
        )?;
        object_table.validate_persistent_payload_refs(&promoted_payload)?;
        self.staged_objects.insert(promoted, promoted_payload);
        Ok(promoted)
    }

    fn rewrite_payload_refs_for_promotion_in_attempt(
        &mut self,
        object_table: &mut ObjectTable,
        payload: ObjectPayload,
        attempt: &mut PromotionAttempt,
    ) -> Result<ObjectPayload> {
        match payload {
            ObjectPayload::Struct(fields) => fields
                .into_iter()
                .map(|value| {
                    self.rewrite_value_ref_for_promotion_in_attempt(object_table, value, attempt)
                })
                .collect::<Result<Vec<_>>>()
                .map(ObjectPayload::Struct),
            ObjectPayload::Array(elements) => elements
                .into_iter()
                .map(|value| {
                    self.rewrite_value_ref_for_promotion_in_attempt(object_table, value, attempt)
                })
                .collect::<Result<Vec<_>>>()
                .map(ObjectPayload::Array),
        }
    }

    fn rewrite_value_ref_for_promotion(
        &mut self,
        object_table: &mut ObjectTable,
        value: ObjectValue,
    ) -> Result<ObjectValue> {
        self.run_promotion_attempt(object_table, |state, object_table, attempt| {
            state.rewrite_value_ref_for_promotion_in_attempt(object_table, value, attempt)
        })
    }

    fn rewrite_value_ref_for_promotion_in_attempt(
        &mut self,
        object_table: &mut ObjectTable,
        value: ObjectValue,
        attempt: &mut PromotionAttempt,
    ) -> Result<ObjectValue> {
        let ObjectValue::Ref(Some(object_id)) = value else {
            return Ok(value);
        };
        if object_table.is_persistent(object_id)? {
            return Ok(ObjectValue::Ref(Some(object_id)));
        }
        Ok(ObjectValue::Ref(Some(
            self.promote_transaction_object_graph_in_attempt(object_table, object_id, attempt)?,
        )))
    }

    fn ordinary_gc_value_to_object_value_in_attempt<A: OrdinaryGcPromotionAdapter>(
        &mut self,
        object_table: &mut ObjectTable,
        value: OrdinaryGcPromotionValue,
        adapter: &mut A,
        attempt: &mut PromotionAttempt,
    ) -> Result<ObjectValue> {
        Ok(match value {
            OrdinaryGcPromotionValue::I31(value) => ObjectValue::I31(value),
            OrdinaryGcPromotionValue::I32(value) => ObjectValue::I32(value),
            OrdinaryGcPromotionValue::I64(value) => ObjectValue::I64(value),
            OrdinaryGcPromotionValue::F32(value) => ObjectValue::F32(value),
            OrdinaryGcPromotionValue::F64(value) => ObjectValue::F64(value),
            OrdinaryGcPromotionValue::V128(value) => ObjectValue::V128(value),
            OrdinaryGcPromotionValue::FuncRef(identity) => ObjectValue::FuncRef(identity),
            OrdinaryGcPromotionValue::ExternRef(identity) => ObjectValue::ExternRef(identity),
            OrdinaryGcPromotionValue::GcRef(None) => ObjectValue::Ref(None),
            OrdinaryGcPromotionValue::GcRef(Some(gc_ref)) => {
                if ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
                    ObjectValue::I31(ObjectTable::decode_raw_i31_ref(u64::from(gc_ref))?)
                } else {
                    let object_id = self
                        .promote_gc_ref_for_persistence_in_attempt(
                            object_table,
                            gc_ref,
                            adapter,
                            attempt,
                        )?
                        .with_context(|| {
                            format!(
                                "ordinary GC ref {gc_ref:#x} is an inline durable value, not a heap object edge"
                            )
                        })?;
                    ObjectValue::Ref(Some(object_id))
                }
            }
        })
    }

    fn promote_gc_ref_for_persistence_in_attempt<A: OrdinaryGcPromotionAdapter>(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: u32,
        adapter: &mut A,
        attempt: &mut PromotionAttempt,
    ) -> Result<Option<ObjectId>> {
        if gc_ref == 0 {
            return Ok(None);
        }
        if let Some(object_id) = object_table.known_object_id_for_live_gc_ref_bridge(gc_ref) {
            if object_table.is_persistent(object_id)? {
                return Ok(Some(object_id));
            }
            return self
                .promote_transaction_object_graph_in_attempt(object_table, object_id, attempt)
                .map(Some);
        }
        if ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
            return Ok(None);
        }
        if let Some(promoted) = self.promoted_gc_refs.get(&gc_ref).copied() {
            return Ok(Some(promoted));
        }
        if self.durable_leaf_gc_refs.contains(&gc_ref) {
            return Ok(None);
        }

        let Some(source) = adapter.promotion_source_for_gc_ref(object_table, gc_ref)? else {
            bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
        };
        match source {
            OrdinaryGcPromotionSource::I31(_)
            | OrdinaryGcPromotionSource::FuncRef(_)
            | OrdinaryGcPromotionSource::ExternRef(_) => {
                if self.durable_leaf_gc_refs.insert(gc_ref) {
                    attempt.record_durable_leaf_gc_ref(gc_ref);
                }
                Ok(None)
            }
            OrdinaryGcPromotionSource::Unsupported(reason) => {
                bail!("ordinary Wasmtime GC reference cannot be promoted: {reason}")
            }
            OrdinaryGcPromotionSource::Struct {
                type_layout_id,
                fields,
            } => {
                let promoted = object_table.reserve_persistent_object_id_for_promotion(
                    ObjectKind::Struct,
                    type_layout_id,
                )?;
                self.record_allocated_object(promoted)?;
                self.promoted_gc_refs.insert(gc_ref, promoted);
                attempt.record_promoted_gc_ref(gc_ref, promoted);
                let fields = fields
                    .into_iter()
                    .map(|value| {
                        self.ordinary_gc_value_to_object_value_in_attempt(
                            object_table,
                            value,
                            adapter,
                            attempt,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                let payload = ObjectPayload::Struct(fields);
                object_table.validate_persistent_payload_refs(&payload)?;
                self.staged_objects.insert(promoted, payload);
                Ok(Some(promoted))
            }
            OrdinaryGcPromotionSource::Array {
                type_layout_id,
                elements,
            } => {
                let promoted = object_table.reserve_persistent_object_id_for_promotion(
                    ObjectKind::Array,
                    type_layout_id,
                )?;
                self.record_allocated_object(promoted)?;
                self.promoted_gc_refs.insert(gc_ref, promoted);
                attempt.record_promoted_gc_ref(gc_ref, promoted);
                let elements = elements
                    .into_iter()
                    .map(|value| {
                        self.ordinary_gc_value_to_object_value_in_attempt(
                            object_table,
                            value,
                            adapter,
                            attempt,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?;
                let payload = ObjectPayload::Array(elements);
                object_table.validate_persistent_payload_refs(&payload)?;
                self.staged_objects.insert(promoted, payload);
                Ok(Some(promoted))
            }
        }
    }

    fn persistent_object_id_for_live_bridge_after_promotion(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: u32,
    ) -> Result<Option<ObjectId>> {
        let mut adapter = NoOrdinaryGcPromotionAdapter;
        self.persistent_object_id_for_live_bridge_after_promotion_with_adapter(
            object_table,
            gc_ref,
            &mut adapter,
        )
    }

    // This is the allowed bridge from an ordinary live Wasmtime GC object into the
    // persistent object graph. It must copy the source into object storage and
    // return an ObjectId. Persistent records after this point must not depend on
    // the original VMGcRef.
    fn persistent_object_id_for_live_bridge_after_promotion_with_adapter<
        A: OrdinaryGcPromotionAdapter,
    >(
        &mut self,
        object_table: &mut ObjectTable,
        gc_ref: u32,
        adapter: &mut A,
    ) -> Result<Option<ObjectId>> {
        self.run_promotion_attempt(object_table, |state, object_table, attempt| {
            state.promote_gc_ref_for_persistence_in_attempt(object_table, gc_ref, adapter, attempt)
        })
    }

    // This validates the result of the live-VMGcRef-to-persistent-ObjectId
    // bridge after promotion has completed. It must return only an already
    // persistent or already promoted ObjectId; persistent records after this
    // point must not depend on the original VMGcRef.
    pub(super) fn persistent_object_id_for_live_bridge_after_completed_promotion(
        &self,
        object_table: &ObjectTable,
        gc_ref: u32,
    ) -> Result<Option<ObjectId>> {
        if gc_ref == 0 {
            return Ok(None);
        }
        if let Some(object_id) = object_table.known_object_id_for_live_gc_ref_bridge(gc_ref) {
            if object_table.is_persistent(object_id)? {
                return Ok(Some(object_id));
            }
            let promoted = self
                .promoted_objects
                .get(&object_id)
                .copied()
                .with_context(|| {
                    format!(
                        "transactional GC ref {gc_ref:#x} backing object {object_id:?} was not promoted before commit"
                    )
                })
                .context(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED)?;
            return Ok(Some(promoted));
        }
        if ObjectTable::is_raw_i31_ref(u64::from(gc_ref)) {
            return Ok(None);
        }
        if let Some(promoted) = self.promoted_gc_refs.get(&gc_ref).copied() {
            return Ok(Some(promoted));
        }
        if self.durable_leaf_gc_refs.contains(&gc_ref) {
            return Ok(None);
        }
        bail!(VOLATILE_GC_REF_PROMOTION_UNIMPLEMENTED);
    }

    fn run_promotion_attempt<T, F>(&mut self, object_table: &mut ObjectTable, f: F) -> Result<T>
    where
        F: FnOnce(&mut Self, &mut ObjectTable, &mut PromotionAttempt) -> Result<T>,
    {
        let mut attempt = PromotionAttempt::new(self);
        match f(self, object_table, &mut attempt) {
            Ok(value) => Ok(value),
            Err(err) => {
                let rollback_context = err.to_string();
                attempt.rollback(self, object_table).with_context(|| {
                    format!("promotion rollback failed after error: {rollback_context}")
                })?;
                Err(err)
            }
        }
    }

    pub(crate) fn promote_extern_ref_for_persistence(
        &mut self,
        _extern_ref: u64,
    ) -> Result<ObjectId> {
        self.ensure_active()?;
        self.abort()?;
        bail!("persistent extern object promotion is not supported")
    }

    pub(crate) fn promote_persistent_references_before_commit(
        &mut self,
        object_table: &mut ObjectTable,
    ) -> Result<bool> {
        let mut adapter = NoOrdinaryGcPromotionAdapter;
        self.promote_persistent_references_before_commit_with_adapter(object_table, &mut adapter)
    }

    pub(crate) fn promote_persistent_references_before_commit_with_adapter<
        A: OrdinaryGcPromotionAdapter,
    >(
        &mut self,
        object_table: &mut ObjectTable,
        adapter: &mut A,
    ) -> Result<bool> {
        self.ensure_active()?;

        let initial_promoted_objects = self.promoted_objects.len();
        let initial_promoted_gc_refs = self.promoted_gc_refs.len();
        let initial_durable_leaf_gc_refs = self.durable_leaf_gc_refs.len();
        let initial_staged_object_count = self.staged_objects.len();
        let mut changed = false;

        let staged_globals = self.staged_globals.values().copied().collect::<Vec<_>>();
        for value in staged_globals {
            if let GlobalSnapshot::GcRef(gc_ref) = value {
                self.persistent_object_id_for_live_bridge_after_promotion_with_adapter(
                    object_table,
                    gc_ref,
                    adapter,
                )?;
            }
        }

        let staged_table_elements = self
            .staged_table_elements
            .values()
            .copied()
            .collect::<Vec<_>>();
        for value in staged_table_elements {
            if let TableElementSnapshot::GcRef(gc_ref) = value {
                self.persistent_object_id_for_live_bridge_after_promotion_with_adapter(
                    object_table,
                    gc_ref,
                    adapter,
                )?;
            }
        }

        let staged_objects = self
            .staged_objects
            .iter()
            .map(|(&object_id, payload)| (object_id, payload.clone()))
            .collect::<Vec<_>>();
        for (object_id, payload) in staged_objects {
            if !object_table.is_persistent(object_id)? {
                continue;
            }
            let rewritten =
                self.rewrite_payload_refs_for_promotion(object_table, payload.clone())?;
            if rewritten != payload {
                self.staged_objects.insert(object_id, rewritten);
                changed = true;
            }
        }

        Ok(changed
            || self.promoted_objects.len() != initial_promoted_objects
            || self.promoted_gc_refs.len() != initial_promoted_gc_refs
            || self.durable_leaf_gc_refs.len() != initial_durable_leaf_gc_refs
            || self.staged_objects.len() != initial_staged_object_count)
    }
}
