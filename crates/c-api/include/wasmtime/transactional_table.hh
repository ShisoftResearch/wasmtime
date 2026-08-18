/**
 * \file wasmtime/transactional_table.hh
 */

#ifndef WASMTIME_TRANSACTIONAL_TABLE_HH
#define WASMTIME_TRANSACTIONAL_TABLE_HH

#include <wasmtime/extern.h>
#include <wasmtime/store.hh>
#include <wasmtime/types/table.hh>

namespace wasmtime {

/// A transactional WebAssembly table.
///
/// It is distinct from `Table`: ordinary host table operations directly access
/// physical storage and therefore cannot operate on this handle.
class TransactionalTable {
  wasmtime_transactional_table_t table;

public:
  /// Creates a transactional table from its raw C API representation.
  TransactionalTable(wasmtime_transactional_table_t table) : table(table) {}

  /// Returns the type of this transactional table.
  TableType type(Store::Context cx) const {
    return TableType(wasmtime_transactional_table_type(cx.ptr, &table));
  }

  /// Returns the raw C API representation of this table.
  const wasmtime_transactional_table_t &capi() const { return table; }
};

} // namespace wasmtime

#endif // WASMTIME_TRANSACTIONAL_TABLE_HH
