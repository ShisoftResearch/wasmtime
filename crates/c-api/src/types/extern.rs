use crate::{CFuncType, CGlobalType, CMemoryType, CTableType, CTagType};
use crate::{wasm_functype_t, wasm_globaltype_t, wasm_memorytype_t, wasm_tabletype_t};
use wasmtime::ExternType;

#[repr(C)]
#[derive(Clone)]
pub struct wasm_externtype_t {
    pub(crate) which: CExternType,
}

wasmtime_c_api_macros::declare_ty!(wasm_externtype_t);

#[derive(Clone)]
pub(crate) enum CExternType {
    Func(CFuncType),
    Global(CGlobalType),
    TransactionalGlobal(CGlobalType),
    Memory(CMemoryType),
    TransactionalMemory(CMemoryType),
    Table(CTableType),
    TransactionalTable(CTableType),
    Tag(CTagType),
}

impl CExternType {
    pub(crate) fn new(ty: ExternType) -> CExternType {
        match ty {
            ExternType::Func(f) => CExternType::Func(CFuncType::new(f)),
            ExternType::Global(f) => CExternType::Global(CGlobalType::new(f)),
            ExternType::TransactionalGlobal(f) => {
                CExternType::TransactionalGlobal(CGlobalType::new(f))
            }
            ExternType::Memory(f) => CExternType::Memory(CMemoryType::new(f)),
            ExternType::TransactionalMemory(f) => {
                CExternType::TransactionalMemory(CMemoryType::new(f))
            }
            ExternType::Table(f) => CExternType::Table(CTableType::new(f)),
            ExternType::TransactionalTable(f) => {
                CExternType::TransactionalTable(CTableType::new(f))
            }
            ExternType::Tag(t) => CExternType::Tag(CTagType::new(t)),
        }
    }
}

pub type wasm_externkind_t = u8;

pub const WASM_EXTERN_FUNC: wasm_externkind_t = 0;
pub const WASM_EXTERN_GLOBAL: wasm_externkind_t = 1;
pub const WASM_EXTERN_TABLE: wasm_externkind_t = 2;
pub const WASM_EXTERN_MEMORY: wasm_externkind_t = 3;
/// Value returned by `wasm_externtype_kind` for exception tags.
/// This extends the `wasm_externkind_t` range (0-3 in wasm.h) with tag support.
pub const WASMTIME_EXTERNTYPE_TAG: wasm_externkind_t = 4;
/// Value returned by `wasm_externtype_kind` for transactional globals.
pub const WASMTIME_EXTERNTYPE_TRANSACTIONAL_GLOBAL: wasm_externkind_t = 5;
/// Value returned by `wasm_externtype_kind` for transactional memories.
pub const WASMTIME_EXTERNTYPE_TRANSACTIONAL_MEMORY: wasm_externkind_t = 6;
/// Value returned by `wasm_externtype_kind` for transactional tables.
pub const WASMTIME_EXTERNTYPE_TRANSACTIONAL_TABLE: wasm_externkind_t = 7;

impl wasm_externtype_t {
    pub(crate) fn from_extern_type(ty: ExternType) -> wasm_externtype_t {
        wasm_externtype_t {
            which: CExternType::new(ty),
        }
    }

    pub(crate) fn from_cextern_type(ty: CExternType) -> wasm_externtype_t {
        wasm_externtype_t { which: ty }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_kind(et: &wasm_externtype_t) -> wasm_externkind_t {
    match &et.which {
        CExternType::Func(_) => WASM_EXTERN_FUNC,
        CExternType::Table(_) => WASM_EXTERN_TABLE,
        CExternType::Global(_) => WASM_EXTERN_GLOBAL,
        CExternType::TransactionalGlobal(_) => WASMTIME_EXTERNTYPE_TRANSACTIONAL_GLOBAL,
        CExternType::Memory(_) => WASM_EXTERN_MEMORY,
        CExternType::TransactionalMemory(_) => WASMTIME_EXTERNTYPE_TRANSACTIONAL_MEMORY,
        CExternType::Tag(_) => WASMTIME_EXTERNTYPE_TAG,
        CExternType::TransactionalTable(_) => WASMTIME_EXTERNTYPE_TRANSACTIONAL_TABLE,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_functype(et: &wasm_externtype_t) -> Option<&wasm_functype_t> {
    wasm_externtype_as_functype_const(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_functype_const(
    et: &wasm_externtype_t,
) -> Option<&wasm_functype_t> {
    wasm_functype_t::try_from(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_globaltype(
    et: &wasm_externtype_t,
) -> Option<&wasm_globaltype_t> {
    wasm_externtype_as_globaltype_const(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_globaltype_const(
    et: &wasm_externtype_t,
) -> Option<&wasm_globaltype_t> {
    wasm_globaltype_t::try_from(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_tabletype(
    et: &wasm_externtype_t,
) -> Option<&wasm_tabletype_t> {
    wasm_externtype_as_tabletype_const(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_tabletype_const(
    et: &wasm_externtype_t,
) -> Option<&wasm_tabletype_t> {
    wasm_tabletype_t::try_from(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_memorytype(
    et: &wasm_externtype_t,
) -> Option<&wasm_memorytype_t> {
    wasm_externtype_as_memorytype_const(et)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_externtype_as_memorytype_const(
    et: &wasm_externtype_t,
) -> Option<&wasm_memorytype_t> {
    wasm_memorytype_t::try_from(et)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{GlobalType, MemoryType, Mutability, RefType, TableType, ValType};

    #[test]
    fn transactional_extern_types_keep_their_c_api_kind() {
        let types = [
            (
                ExternType::TransactionalGlobal(GlobalType::new(ValType::I32, Mutability::Const)),
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_GLOBAL,
            ),
            (
                ExternType::TransactionalMemory(MemoryType::new(0, None)),
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_MEMORY,
            ),
            (
                ExternType::TransactionalTable(TableType::new(RefType::FUNCREF, 0, None)),
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_TABLE,
            ),
        ];

        for (ty, kind) in types {
            let ty = wasm_externtype_t::from_extern_type(ty);
            assert_eq!(wasm_externtype_kind(&ty), kind);
            match kind {
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_GLOBAL => {
                    assert!(wasm_externtype_as_globaltype_const(&ty).is_some());
                    assert!(wasm_externtype_as_tabletype_const(&ty).is_none());
                    assert!(wasm_externtype_as_memorytype_const(&ty).is_none());
                }
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_MEMORY => {
                    assert!(wasm_externtype_as_globaltype_const(&ty).is_none());
                    assert!(wasm_externtype_as_tabletype_const(&ty).is_none());
                    assert!(wasm_externtype_as_memorytype_const(&ty).is_some());
                }
                WASMTIME_EXTERNTYPE_TRANSACTIONAL_TABLE => {
                    assert!(wasm_externtype_as_globaltype_const(&ty).is_none());
                    assert!(wasm_externtype_as_tabletype_const(&ty).is_some());
                    assert!(wasm_externtype_as_memorytype_const(&ty).is_none());
                }
                _ => unreachable!(),
            }
        }
    }
}
