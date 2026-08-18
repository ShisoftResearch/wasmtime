/**
 * \file wasmtime/transactional_memory.hh
 */

#ifndef WASMTIME_TRANSACTIONAL_MEMORY_HH
#define WASMTIME_TRANSACTIONAL_MEMORY_HH

#include <wasmtime/error.hh>
#include <wasmtime/memory.h>
#include <wasmtime/store.hh>
#include <wasmtime/types/memory.hh>

namespace wasmtime {

/// A transactional WebAssembly memory with a copy-oriented host API.
///
/// Unlike `Memory`, transactional memory storage is not necessarily
/// contiguous and does not expose a stable host pointer.
class TransactionalMemory {
  wasmtime_transactional_memory_t memory;

public:
  /// Creates a transactional memory from its raw C API representation.
  TransactionalMemory(wasmtime_transactional_memory_t memory) : memory(memory) {}

  /// Returns the type of this transactional memory.
  MemoryType type(Store::Context cx) const {
    return MemoryType(wasmtime_transactional_memory_type(cx.ptr, &memory));
  }

  /// Returns the committed size, in WebAssembly pages.
  uint64_t size(Store::Context cx) const {
    return wasmtime_transactional_memory_size(cx.ptr, &memory);
  }

  /// Returns the committed byte length.
  size_t data_size(Store::Context cx) const {
    return wasmtime_transactional_memory_data_size(cx.ptr, &memory);
  }

  /// Copies committed bytes from this memory into `buffer`.
  Result<std::monostate> read(Store::Context cx, size_t offset,
                              uint8_t *buffer, size_t buffer_len) const {
    auto *error = wasmtime_transactional_memory_read(cx.ptr, &memory, offset,
                                                      buffer, buffer_len);
    if (error != nullptr) {
      return Error(error);
    }
    return std::monostate();
  }

  /// Writes bytes to this memory and commits the write.
  Result<std::monostate> write(Store::Context cx, size_t offset,
                               const uint8_t *buffer, size_t buffer_len) const {
    auto *error = wasmtime_transactional_memory_write(
        cx.ptr, &memory, offset, buffer, buffer_len);
    if (error != nullptr) {
      return Error(error);
    }
    return std::monostate();
  }

  /// Grows this memory and commits the growth.
  Result<uint64_t> grow(Store::Context cx, uint64_t delta) const {
    uint64_t previous = 0;
    auto *error = wasmtime_transactional_memory_grow(cx.ptr, &memory, delta,
                                                      &previous);
    if (error != nullptr) {
      return Error(error);
    }
    return previous;
  }

  /// Returns the raw C API representation of this memory.
  const wasmtime_transactional_memory_t &capi() const { return memory; }
};

} // namespace wasmtime

#endif // WASMTIME_TRANSACTIONAL_MEMORY_HH
