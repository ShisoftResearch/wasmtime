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
            return Ok(());
        }
        self.funcs_by_vm_func_ref.insert(vm_func_ref_addr, identity);
        Ok(())
    }

    pub(crate) fn resolve_func_ref(&self, vm_func_ref_addr: usize) -> Option<DurableFuncIdentity> {
        self.funcs_by_vm_func_ref.get(&vm_func_ref_addr).copied()
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
