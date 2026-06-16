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
    next_live_func_identity_index: u32,
    allow_live_wast_reference_fallbacks: bool,
}

impl DurableReferenceRegistry {
    const LIVE_FUNC_FALLBACK_FINGERPRINT: u64 = 0xffff_ffff_5458_4655;
    const LIVE_EXTERN_FALLBACK_NAMESPACE: u32 = u32::MAX;

    pub(crate) fn enable_live_wast_reference_fallbacks_for_test(&mut self) {
        self.allow_live_wast_reference_fallbacks = true;
    }

    pub(crate) fn live_wast_reference_fallbacks_enabled(&self) -> bool {
        self.allow_live_wast_reference_fallbacks
    }

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

    pub(crate) fn resolve_or_register_live_func_ref(
        &mut self,
        vm_func_ref_addr: usize,
    ) -> Result<DurableFuncIdentity> {
        if let Some(identity) = self.resolve_func_ref(vm_func_ref_addr) {
            return Ok(identity);
        }
        ensure!(
            self.allow_live_wast_reference_fallbacks,
            "ordinary GC promotion cannot encode function reference without registered durable function identity"
        );
        let function_index = self.next_live_func_identity_index;
        self.next_live_func_identity_index = self
            .next_live_func_identity_index
            .checked_add(1)
            .context("live durable function fallback identity overflow")?;
        let identity = DurableFuncIdentity {
            module_fingerprint: Self::LIVE_FUNC_FALLBACK_FINGERPRINT,
            function_index,
            type_layout_id: TypeLayoutId::BUILTIN_FUNC,
        };
        // SHISOFT-TWASM-MOCK: this is a live-only fallback for internal
        // proposal-WAST `tref.tfunc` values that do not yet have a
        // module/function-index durable namespace at the helper boundary. It
        // resolves inside this store only and must be replaced by the final
        // symbolic function identity path before claiming funcref recovery.
        self.register_func_ref(vm_func_ref_addr, identity)?;
        Ok(identity)
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

    pub(crate) fn resolve_or_register_live_extern_ref(
        &mut self,
        raw_gc_ref: u32,
    ) -> Result<DurableExternIdentity> {
        if let Some(identity) = self.resolve_extern_ref(raw_gc_ref) {
            return Ok(identity);
        }
        ensure!(
            self.allow_live_wast_reference_fallbacks,
            "ordinary GC promotion cannot encode external reference without embedded durable external identity"
        );
        let identity = DurableExternIdentity {
            namespace: Self::LIVE_EXTERN_FALLBACK_NAMESPACE,
            handle: u64::from(raw_gc_ref),
            type_layout_id: TypeLayoutId::BUILTIN_EXTERN,
        };
        // SHISOFT-TWASM-MOCK: proposal WAST `tref.textern` values are ordinary
        // Wasmtime host externrefs, not persistent external objects. Register a
        // store-local identity so live transactional tables/globals/arrays can
        // roundtrip them. This is not a restart-stable external namespace.
        self.register_extern_ref(raw_gc_ref, identity)?;
        Ok(identity)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_wast_reference_fallbacks_are_disabled_by_default() {
        let mut registry = DurableReferenceRegistry::default();

        assert!(registry.resolve_or_register_live_func_ref(0x1234).is_err());
        assert!(registry.resolve_or_register_live_extern_ref(0x55).is_err());
    }

    #[test]
    fn live_wast_reference_fallbacks_require_explicit_opt_in() -> Result<()> {
        let mut registry = DurableReferenceRegistry::default();
        registry.enable_live_wast_reference_fallbacks_for_test();

        let func = registry.resolve_or_register_live_func_ref(0x1234)?;
        assert_eq!(registry.resolve_func_ref(0x1234), Some(func));
        assert_eq!(registry.resolve_func_identity(func), Some(0x1234));

        let extern_ = registry.resolve_or_register_live_extern_ref(0x55)?;
        assert_eq!(registry.resolve_extern_ref(0x55), Some(extern_));
        assert_eq!(registry.resolve_extern_identity(extern_), Some(0x55));

        Ok(())
    }
}
