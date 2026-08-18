/**
 * \file wasmtime/types/extern.hh
 */

#ifndef WASMTIME_TYPES_EXTERN_HH
#define WASMTIME_TYPES_EXTERN_HH

#include <variant>
#include <wasm.h>
#include <wasmtime/types/export.hh>
#include <wasmtime/types/func.hh>
#include <wasmtime/types/global.hh>
#include <wasmtime/types/import.hh>
#include <wasmtime/types/memory.hh>
#include <wasmtime/types/table.hh>
#include <wasmtime/types/tag.hh>

namespace wasmtime {

/**
 * \brief Generic type of a WebAssembly item.
 */
class ExternType {
  friend class ExportType;
  friend class ImportType;

public:
  /// \brief Type information for a native transactional global.
  class TransactionalGlobalRef {
    GlobalType::Ref type_;

  public:
    explicit TransactionalGlobalRef(const wasm_globaltype_t *type) : type_(type) {}
    /// Returns whether this global is mutable.
    bool is_mutable() const { return type_.is_mutable(); }
    /// Returns the type of value stored within this global.
    ValType::Ref content() const { return type_.content(); }
  };

  /// \brief Type information for a native transactional memory.
  class TransactionalMemoryRef {
    MemoryType::Ref type_;

  public:
    explicit TransactionalMemoryRef(const wasm_memorytype_t *type) : type_(type) {}
    /// Returns the minimum size, in pages.
    uint64_t min() const { return type_.min(); }
    /// Returns the maximum size, if specified.
    std::optional<uint64_t> max() const { return type_.max(); }
    /// Returns whether this is a 64-bit memory.
    bool is_64() const { return type_.is_64(); }
    /// Returns whether this memory is shared.
    bool is_shared() const { return type_.is_shared(); }
    /// Returns its page size, in bytes.
    uint64_t page_size() const { return type_.page_size(); }
  };

  /// \brief Type information for a native transactional table.
  class TransactionalTableRef {
    TableType::Ref type_;

  public:
    explicit TransactionalTableRef(const wasm_tabletype_t *type) : type_(type) {}
    /// Returns the minimum table size.
    uint32_t min() const { return type_.min(); }
    /// Returns the maximum table size, if specified.
    std::optional<uint32_t> max() const { return type_.max(); }
    /// Returns the element type.
    ValType::Ref element() const { return type_.element(); }
  };

  /// \typedef Ref
  /// \brief Non-owning reference to an item's type
  ///
  /// This cannot be used after the original owner has been deleted, and
  /// otherwise this is used to determine what the actual type of the outer item
  /// is.
  typedef std::variant<FuncType::Ref, GlobalType::Ref, TableType::Ref,
                       MemoryType::Ref, TagType::Ref, TransactionalGlobalRef,
                       TransactionalMemoryRef, TransactionalTableRef>
      Ref;

  /// Extract the type of the item imported by the provided type.
  static Ref from_import(ImportType::Ref ty) {
    // TODO: this would ideally be some sort of implicit constructor, unsure how
    // to do that though...
    return ref_from_c(ty.raw_type());
  }

  /// Extract the type of the item exported by the provided type.
  static Ref from_export(ExportType::Ref ty) {
    // TODO: this would ideally be some sort of implicit constructor, unsure how
    // to do that though...
    return ref_from_c(ty.raw_type());
  }

private:
  static Ref ref_from_c(const wasm_externtype_t *ptr) {
    switch (wasm_externtype_kind(ptr)) {
    case WASM_EXTERN_FUNC:
      return wasm_externtype_as_functype_const(ptr);
    case WASM_EXTERN_GLOBAL:
      return wasm_externtype_as_globaltype_const(ptr);
    case WASM_EXTERN_TABLE:
      return wasm_externtype_as_tabletype_const(ptr);
    case WASM_EXTERN_MEMORY:
      return wasm_externtype_as_memorytype_const(ptr);
    case WASM_EXTERN_TAG:
      return wasm_externtype_as_tagtype_const(ptr);
    case WASMTIME_EXTERNTYPE_TRANSACTIONAL_GLOBAL:
      return TransactionalGlobalRef(wasm_externtype_as_globaltype_const(ptr));
    case WASMTIME_EXTERNTYPE_TRANSACTIONAL_MEMORY:
      return TransactionalMemoryRef(wasm_externtype_as_memorytype_const(ptr));
    case WASMTIME_EXTERNTYPE_TRANSACTIONAL_TABLE:
      return TransactionalTableRef(wasm_externtype_as_tabletype_const(ptr));
    }
    std::abort();
  }
};

}; // namespace wasmtime

#endif // WASMTIME_TYPES_EXTERN_HH
