/**
 * \file wasmtime/transactional_global.hh
 */

#ifndef WASMTIME_TRANSACTIONAL_GLOBAL_HH
#define WASMTIME_TRANSACTIONAL_GLOBAL_HH

#include <wasmtime/extern.h>
#include <wasmtime/store.hh>
#include <wasmtime/types/global.hh>

namespace wasmtime {

/// A transactional WebAssembly global.
///
/// It is distinct from `Global`: ordinary host global operations directly
/// access physical storage and therefore cannot operate on this handle.
class TransactionalGlobal {
  wasmtime_transactional_global_t global;

public:
  /// Creates a transactional global from its raw C API representation.
  TransactionalGlobal(wasmtime_transactional_global_t global) : global(global) {}

  /// Returns the type of this transactional global.
  GlobalType type(Store::Context cx) const {
    return GlobalType(wasmtime_transactional_global_type(cx.ptr, &global));
  }

  /// Returns the raw C API representation of this global.
  const wasmtime_transactional_global_t &capi() const { return global; }
};

} // namespace wasmtime

#endif // WASMTIME_TRANSACTIONAL_GLOBAL_HH
