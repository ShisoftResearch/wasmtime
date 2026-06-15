use super::type_layout::TypeLayoutId;
use crate::prelude::*;
use alloc::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DurableFuncIdentity {
    pub(crate) module_fingerprint: u64,
    pub(crate) function_index: u32,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct DurableExternIdentity {
    pub(crate) namespace: u32,
    pub(crate) handle: u64,
    pub(crate) type_layout_id: TypeLayoutId,
}

#[derive(Default)]
pub(crate) struct DurableReferenceRegistry {
    funcs_by_vm_func_ref: BTreeMap<usize, DurableFuncIdentity>,
    vm_func_refs_by_identity: BTreeMap<DurableFuncIdentity, usize>,
    externs_by_raw_gc_ref: BTreeMap<u32, DurableExternIdentity>,
    raw_gc_refs_by_extern_identity: BTreeMap<DurableExternIdentity, u32>,
}

impl DurableReferenceRegistry {
    pub(crate) fn register_func_ref(
        &mut self,
        vm_func_ref_addr: usize,
        identity: DurableFuncIdentity,
    ) -> Result<()> {
        if let Some(existing) = self.funcs_by_vm_func_ref.get(&vm_func_ref_addr) {
            ensure!(
                *existing == identity,
                "durable function reference identity registration conflict"
            );
        }
        if let Some(existing) = self.vm_func_refs_by_identity.get(&identity) {
            ensure!(
                *existing == vm_func_ref_addr,
                "durable function identity resolves to multiple function references"
            );
        }
        if self.funcs_by_vm_func_ref.contains_key(&vm_func_ref_addr)
            && self.vm_func_refs_by_identity.contains_key(&identity)
        {
            return Ok(());
        }
        self.funcs_by_vm_func_ref.insert(vm_func_ref_addr, identity);
        self.vm_func_refs_by_identity
            .insert(identity, vm_func_ref_addr);
        Ok(())
    }

    pub(crate) fn resolve_func_ref(&self, vm_func_ref_addr: usize) -> Option<DurableFuncIdentity> {
        self.funcs_by_vm_func_ref.get(&vm_func_ref_addr).copied()
    }

    pub(crate) fn resolve_func_identity(&self, identity: DurableFuncIdentity) -> Option<usize> {
        self.vm_func_refs_by_identity.get(&identity).copied()
    }

    pub(crate) fn register_extern_ref(
        &mut self,
        raw_gc_ref: u32,
        identity: DurableExternIdentity,
    ) -> Result<()> {
        ensure!(
            raw_gc_ref != 0,
            "durable external reference cannot register null externref"
        );
        if let Some(existing) = self.externs_by_raw_gc_ref.get(&raw_gc_ref) {
            ensure!(
                *existing == identity,
                "durable external reference identity registration conflict"
            );
        }
        if let Some(existing) = self.raw_gc_refs_by_extern_identity.get(&identity) {
            ensure!(
                *existing == raw_gc_ref,
                "durable external identity resolves to multiple external references"
            );
        }
        if self.externs_by_raw_gc_ref.contains_key(&raw_gc_ref)
            && self.raw_gc_refs_by_extern_identity.contains_key(&identity)
        {
            return Ok(());
        }
        self.externs_by_raw_gc_ref.insert(raw_gc_ref, identity);
        self.raw_gc_refs_by_extern_identity
            .insert(identity, raw_gc_ref);
        Ok(())
    }

    pub(crate) fn resolve_extern_ref(&self, raw_gc_ref: u32) -> Option<DurableExternIdentity> {
        self.externs_by_raw_gc_ref.get(&raw_gc_ref).copied()
    }

    pub(crate) fn resolve_extern_identity(&self, identity: DurableExternIdentity) -> Option<u32> {
        self.raw_gc_refs_by_extern_identity.get(&identity).copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DurableExternRefHostData {
    identity: DurableExternIdentity,
}

impl DurableExternRefHostData {
    pub(crate) fn new(identity: DurableExternIdentity) -> Self {
        Self { identity }
    }

    pub(crate) fn identity(&self) -> DurableExternIdentity {
        self.identity
    }
}
