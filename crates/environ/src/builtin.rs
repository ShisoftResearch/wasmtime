/// Helper macro to iterate over all builtin functions and their signatures.
#[macro_export]
macro_rules! foreach_builtin_function {
    ($mac:ident) => {
        $mac! {
            // Returns an index for wasm's `memory.grow` builtin function.
            memory_grow(vmctx: vmctx, delta: u64, index: u32) -> pointer;
            // Begins a transactional WebAssembly transaction for `tfunc` entry
            // if no transaction is already active.
            transaction_enter_tfunc(vmctx: vmctx) -> u64;
            // Begins a transactional WebAssembly transaction.
            transaction_begin(vmctx: vmctx) -> bool;
            // Commits a transactional WebAssembly transaction.
            transaction_commit(vmctx: vmctx) -> bool;
            // Fails and aborts a transactional WebAssembly transaction.
            transaction_fail(vmctx: vmctx) -> bool;
            // Returns a pointer to a staged transactional global cell.
            transaction_tglobal_get(vmctx: vmctx, global: u32) -> pointer;
            // Stages a transactional global write. `tag` identifies the value type.
            transaction_tglobal_set(vmctx: vmctx, global: u32, tag: u32, value: u64) -> bool;
            // Stages a transactional v128 global write from a 16-byte value pointer.
            transaction_tglobal_set_v128(vmctx: vmctx, global: u32, value: pointer) -> bool;
            // Returns a pointer to readable transactional memory bytes.
            transaction_tmemory_load(vmctx: vmctx, memory: u32, addr: u64, offset: u64, len: u32) -> pointer;
            // Returns a pointer to writable staged transactional memory bytes.
            transaction_tmemory_store(vmctx: vmctx, memory: u32, addr: u64, offset: u64, len: u32) -> pointer;
            // Returns the visible transactional memory size.
            transaction_tmemory_size(vmctx: vmctx, memory: u32) -> pointer;
            // Stages a transactional memory grow and returns the previous visible size.
            transaction_tmemory_grow(vmctx: vmctx, memory: u32, delta: u64) -> pointer;
            // Stages a transactional memory fill.
            transaction_tmemory_fill(vmctx: vmctx, memory: u32, dst: u64, val: u32, len: u64) -> bool;
            // Stages a transactional memory copy.
            transaction_tmemory_copy(vmctx: vmctx, dst_memory: u32, src_memory: u32, dst: u64, src: u64, len: u64) -> bool;
            // Stages a transactional memory initialization from runtime data bytes.
            transaction_tmemory_init(vmctx: vmctx, memory: u32, dst: u64, src: u64, len: u64, data: pointer, data_len: u64) -> bool;
            // Initializes committed transactional memory from active data during module startup.
            transaction_tmemory_static_init(vmctx: vmctx, memory: u32, dst: u64, len: u64, data: pointer, data_len: u64) -> bool;
            // Checks transactional context before lowering applies `tdata.drop`.
            transaction_tdata_drop(vmctx: vmctx, data: u32) -> bool;
            // Returns a transactional table element.
            transaction_ttable_get(vmctx: vmctx, table: u32, index: u64) -> pointer;
            // Stages a transactional table element write.
            transaction_ttable_set(vmctx: vmctx, table: u32, index: u64, value: pointer) -> bool;
            // Acquires a readable transactional table range.
            transaction_ttable_read_range(vmctx: vmctx, table: u32, start: u64, len: u64) -> bool;
            // Acquires a writable transactional table range.
            transaction_ttable_write_range(vmctx: vmctx, table: u32, start: u64, len: u64) -> bool;
            // Returns the visible transactional table size.
            transaction_ttable_size(vmctx: vmctx, table: u32) -> pointer;
            // Grows a transactional table and returns the previous visible size.
            transaction_ttable_grow(vmctx: vmctx, table: u32, delta: u64) -> pointer;
            // Associates a newly allocated Wasmtime GC struct with a transactional object record.
            transaction_tstruct_new(vmctx: vmctx, gc_ref: u32, struct_type: u32, field_count: u32, fields: pointer) -> bool;
            // Stages a transactional struct field write.
            transaction_tstruct_set(vmctx: vmctx, gc_ref: u32, field: u32, tag: u32, low: u64, high: u64) -> bool;
            // Reads a transactional struct field as an ObjectValueAbi scratch pointer.
            transaction_tstruct_get(vmctx: vmctx, gc_ref: u32, field: u32) -> pointer;
            // Returns an index for wasm's `memory.copy`
            memory_copy(vmctx: vmctx, dst: pointer, src: pointer, len: size);
            // Returns an index for wasm's `memory.fill` instruction.
            memory_fill(vmctx: vmctx, dst: pointer, val: u32, len: size);
            // Returns the current size of the passive `elem` segment.
            passive_elem_segment_len(vmctx: vmctx, elem: u32) -> size;
            // Returns the base address of the passive `elem` segment.
            passive_elem_segment_base(vmctx: vmctx, elem: u32) -> pointer;
            // Guts of `elem.drop` for passive data segments.
            passive_elem_segment_drop(vmctx: vmctx, elem: u32) -> bool;
            // Returns a value for wasm's `ref.func` instruction.
            ref_func(vmctx: vmctx, func: u32) -> pointer;
            // Returns a table entry after lazily initializing it.
            table_get_lazy_init_func_ref(vmctx: vmctx, table: u32, index: u64) -> pointer;
            // Grows `table` by `delta` elements, returning the destination
            // address that new elements should be written at.
            table_grow(vmctx: vmctx, table: u32, delta: u64) -> pointer;
            // Returns an index for wasm's `memory.atomic.notify` instruction.
            #[cfg(feature = "threads")]
            memory_atomic_notify(vmctx: vmctx, memory: u32, addr: u64, count: u32) -> u64;
            // Returns an index for wasm's `memory.atomic.wait32` instruction.
            #[cfg(feature = "threads")]
            memory_atomic_wait32(vmctx: vmctx, memory: u32, addr: u64, expected: u32, timeout: u64) -> u64;
            // Returns an index for wasm's `memory.atomic.wait64` instruction.
            #[cfg(feature = "threads")]
            memory_atomic_wait64(vmctx: vmctx, memory: u32, addr: u64, expected: u64, timeout: u64) -> u64;
            // Invoked when fuel has run out while executing a function.
            out_of_gas(vmctx: vmctx) -> bool;
            // Invoked when we reach a new epoch.
            #[cfg(target_has_atomic = "64")]
            new_epoch(vmctx: vmctx) -> u64;
            // Invoked before malloc returns.
            #[cfg(feature = "wmemcheck")]
            check_malloc(vmctx: vmctx, addr: u32, len: u32) -> bool;
            // Invoked before the free returns.
            #[cfg(feature = "wmemcheck")]
            check_free(vmctx: vmctx, addr: u32) -> bool;
            // Invoked before a load is executed.
            #[cfg(feature = "wmemcheck")]
            check_load(vmctx: vmctx, num_bytes: u32, addr: u32, offset: u32) -> bool;
            // Invoked before a store is executed.
            #[cfg(feature = "wmemcheck")]
            check_store(vmctx: vmctx, num_bytes: u32, addr: u32, offset: u32) -> bool;
            // Invoked after malloc is called.
            #[cfg(feature = "wmemcheck")]
            malloc_start(vmctx: vmctx);
            // Invoked after free is called.
            #[cfg(feature = "wmemcheck")]
            free_start(vmctx: vmctx);
            // Invoked when wasm stack pointer is updated.
            #[cfg(feature = "wmemcheck")]
            update_stack_pointer(vmctx: vmctx, value: u32);
            // Invoked before memory.grow is called.
            #[cfg(feature = "wmemcheck")]
            update_mem_size(vmctx: vmctx, num_bytes: u32);

            // Drop a non-stack GC reference (eg an overwritten table entry)
            // once it will no longer be used again. (Note: `val` is not of type
            // `reference` because it needn't appear in any stack maps, as it
            // must not be live after this call.)
            #[cfg(feature = "gc-drc")]
            drop_gc_ref(vmctx: vmctx, val: u32);

            // Grow the GC heap by `bytes_needed` bytes.
            //
            // Traps if growing the GC heap fails.
            #[cfg(feature = "gc-null")]
            grow_gc_heap(vmctx: vmctx, bytes_needed: u64) -> bool;

            // Allocate a new, uninitialized GC object and return a reference to
            // it.
            #[cfg(any(feature = "gc-drc", feature = "gc-copying"))]
            gc_alloc_raw(
                vmctx: vmctx,
                kind: u32,
                shared_type_index: u32,
                size: u32,
                align: u32
            ) -> u32;

            // Intern a `funcref` into the GC heap, returning its
            // `FuncRefTableId`.
            //
            // This libcall may not GC.
            #[cfg(feature = "gc")]
            intern_func_ref_for_gc_heap(
                vmctx: vmctx,
                func_ref: pointer
            ) -> u64;

            // Get the raw `VMFuncRef` pointer associated with a
            // `FuncRefTableId` from an earlier `intern_func_ref_for_gc_heap`
            // call.
            //
            // This libcall may not GC.
            //
            // Passes in the `ModuleInternedTypeIndex` of the funcref's expected
            // type, or `ModuleInternedTypeIndex::reserved_value()` if we are
            // getting the function reference as an untyped `funcref` rather
            // than a typed `(ref $ty)`.
            //
            // TODO: We will want to eventually expose the table directly to
            // Wasm code, so that it doesn't need to make a libcall to go from
            // id to `VMFuncRef`. That will be a little tricky: it will also
            // require updating the pointer to the slab in the `VMContext` (or
            // `VMStoreContext` or wherever we put it) when the slab is
            // resized.
            #[cfg(feature = "gc")]
            get_interned_func_ref(
                vmctx: vmctx,
                func_ref_id: u32,
                module_interned_type_index: u32
            ) -> pointer;

            // Returns whether `actual_engine_type` is a subtype of
            // `expected_engine_type`.
            #[cfg(feature = "gc")]
            is_subtype(
                vmctx: vmctx,
                actual_engine_type: u32,
                expected_engine_type: u32
            ) -> u32;

            // Wasm floating-point routines for when the CPU instructions aren't available.
            ceil_f32(vmctx: vmctx, x: f32) -> f32;
            ceil_f64(vmctx: vmctx, x: f64) -> f64;
            floor_f32(vmctx: vmctx, x: f32) -> f32;
            floor_f64(vmctx: vmctx, x: f64) -> f64;
            trunc_f32(vmctx: vmctx, x: f32) -> f32;
            trunc_f64(vmctx: vmctx, x: f64) -> f64;
            nearest_f32(vmctx: vmctx, x: f32) -> f32;
            nearest_f64(vmctx: vmctx, x: f64) -> f64;
            i8x16_swizzle(vmctx: vmctx, a: i8x16, b: i8x16) -> i8x16;
            i8x16_shuffle(vmctx: vmctx, a: i8x16, b: i8x16, c: i8x16) -> i8x16;
            fma_f32x4(vmctx: vmctx, x: f32x4, y: f32x4, z: f32x4) -> f32x4;
            fma_f64x2(vmctx: vmctx, x: f64x2, y: f64x2, z: f64x2) -> f64x2;

            // Raises an unconditional trap with the specified code.
            //
            // This is used when signals-based-traps are disabled for backends
            // when an illegal instruction can't be executed for example.
            trap(vmctx: vmctx, code: u8);

            // Raises an unconditional trap where the trap information must have
            // been previously filled in.
            raise(vmctx: vmctx);

            // Creates a new continuation from a funcref.
            #[cfg(feature = "stack-switching")]
            cont_new(vmctx: vmctx, r: pointer, param_count: u32, result_count: u32) -> pointer;

            // Return the instance ID for a given vmctx.
            #[cfg(feature = "gc")]
            get_instance_id(vmctx: vmctx) -> u32;

            // Throw an exception.
            #[cfg(feature = "gc")]
            throw_ref(vmctx: vmctx, exnref: u32) -> bool;

            // Force a GC cycle for the DRC collector.
            #[cfg(feature = "gc-drc")]
            force_gc(vmctx: vmctx) -> bool;

            // Process a debug breakpoint.
            breakpoint(vmctx: vmctx) -> bool;
        }
    };
}

/// Helper macro to define a builtin type such as `BuiltinFunctionIndex` and
/// `ComponentBuiltinFunctionIndex` using the iterator macro, e.g.
/// `foreach_builtin_function`, as the way to generate accessor methods.
macro_rules! declare_builtin_index {
    (
        $(#[$attr:meta])*
        pub struct $index_name:ident : $for_each_builtin:ident ;
    ) => {
        $(#[$attr])*
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $index_name(u32);

        impl core::fmt::Debug for $index_name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct(stringify!($index_name))
                    .field("index", &self.0)
                    .field("ctor", &self.ctor_name())
                    .finish()
            }
        }

        impl $index_name {
            /// Create a new builtin from its raw index
            pub const fn from_u32(i: u32) -> Self {
                assert!(i < Self::len());
                Self(i)
            }

            /// Return the index as an u32 number.
            pub const fn index(&self) -> u32 {
                self.0
            }

            $for_each_builtin!(define_ctor_name);

            $for_each_builtin!(declare_builtin_index_constructors);
        }

        #[cfg(test)]
        impl arbitrary::Arbitrary<'_> for $index_name {
            fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
                Ok(Self(u.int_in_range(0..=Self::len() - 1)?))
            }
        }
    };
}

/// Helper macro used by the above macro.
macro_rules! define_ctor_name {
    (
        $(
            $( #[$attr:meta] )*
                $name:ident( $( $pname:ident: $param:ident ),* ) $( -> $result:ident )?;
        )*
    ) => {
        /// Returns the name of the constructor that creates this index.
        pub fn ctor_name(&self) -> &'static str {
            let mut _i = self.0;
            $(
                if _i == 0 {
                    return stringify!($name);
                }
                _i -= 1;
            )*
            unreachable!()
        }
    }
}

/// Helper macro used by the above macro.
macro_rules! declare_builtin_index_constructors {
    (
        $(
            $( #[$attr:meta] )*
            $name:ident( $( $pname:ident: $param:ident ),* ) $( -> $result:ident )?;
        )*
    ) => {
        declare_builtin_index_constructors!(
            @indices;
            0;
            $( $( #[$attr] )* $name; )*
        );

        /// Returns a symbol name for this builtin.
        pub fn name(&self) -> &'static str {
            $(
                if *self == Self::$name() {
                    return stringify!($name);
                }
            )*
            unreachable!()
        }
    };

    // Base case: no more indices to declare, so define the total number of
    // function indices.
    (
        @indices;
        $len:expr;
    ) => {
        /// Returns the total number of builtin functions.
        pub const fn len() -> u32 {
            $len
        }
    };

    // Recursive case: declare the next index, and then keep declaring the rest of
    // the indices.
    (
         @indices;
         $index:expr;
         $( #[$this_attr:meta] )*
         $this_name:ident;
         $(
             $( #[$rest_attr:meta] )*
             $rest_name:ident;
         )*
    ) => {
        #[expect(missing_docs, reason = "macro-generated")]
        pub const fn $this_name() -> Self {
            Self($index)
        }

        declare_builtin_index_constructors!(
            @indices;
            ($index + 1);
            $( $( #[$rest_attr] )* $rest_name; )*
        );
    }
}

// Define `struct BuiltinFunctionIndex`
declare_builtin_index! {
    /// An index type for builtin functions.
    pub struct BuiltinFunctionIndex : foreach_builtin_function;
}

/// Return value of [`BuiltinFunctionIndex::trap_sentinel`].
pub enum TrapSentinel {
    /// A falsy or zero value indicates a trap.
    Falsy,
    /// The value `-2` indicates a trap (used for growth-related builtins).
    NegativeTwo,
    /// The value `-1` indicates a trap .
    NegativeOne,
    /// Any negative value indicates a trap.
    Negative,
}

impl BuiltinFunctionIndex {
    /// Describes the return value of this builtin and what represents a trap.
    ///
    /// Libcalls don't raise traps themselves and instead delegate to compilers
    /// to do so. This means that some return values of libcalls indicate a trap
    /// is happening and this is represented with sentinel values. This function
    /// returns the description of the sentinel value which indicates a trap, if
    /// any. If `None` is returned from this function then this builtin cannot
    /// generate a trap.
    #[allow(unreachable_code, unused_macro_rules, reason = "macro-generated code")]
    pub fn trap_sentinel(&self) -> Option<TrapSentinel> {
        macro_rules! trap_sentinel {
            (
                $(
                    $( #[$attr:meta] )*
                    $name:ident( $( $pname:ident: $param:ident ),* ) $( -> $result:ident )?;
                )*
            ) => {{
                $(
                    $(#[$attr])*
                    if *self == BuiltinFunctionIndex::$name() {
                        let mut _ret = None;
                        $(_ret = Some(trap_sentinel!(@get $name $result));)?
                        return _ret;
                    }
                )*

                None
            }};

            // Growth-related functions return -2 as a sentinel.
            (@get memory_grow pointer) => (TrapSentinel::NegativeTwo);
            (@get table_grow pointer) => (TrapSentinel::NegativeTwo);
            (@get transaction_tmemory_grow pointer) => (TrapSentinel::NegativeTwo);
            (@get transaction_ttable_grow pointer) => (TrapSentinel::NegativeTwo);

            // Atomics-related functions return a negative value to indicate a trap.
            (@get memory_atomic_notify u64) => (TrapSentinel::Negative);
            (@get memory_atomic_wait32 u64) => (TrapSentinel::Negative);
            (@get memory_atomic_wait64 u64) => (TrapSentinel::Negative);

            // GC allocation functions return a u32 which is zero to indicate a
            // trap.
            (@get gc_alloc_raw u32) => (TrapSentinel::Falsy);
            (@get array_new_data u32) => (TrapSentinel::Falsy);
            (@get array_new_elem u32) => (TrapSentinel::Falsy);

            // The final epoch represents a trap
            (@get new_epoch u64) => (TrapSentinel::NegativeOne);

            // Failure here indicates GC heap corruption.
            (@get get_interned_func_ref pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_enter_tfunc u64) => (TrapSentinel::NegativeOne);
            (@get transaction_tglobal_get pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_tmemory_load pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_tmemory_store pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_tmemory_size pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_ttable_get pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_ttable_size pointer) => (TrapSentinel::NegativeOne);
            (@get transaction_tstruct_get pointer) => (TrapSentinel::NegativeOne);

            // These libcalls can't trap
            (@get ref_func pointer) => (return None);
            (@get table_get_lazy_init_func_ref pointer) => (return None);
            (@get intern_func_ref_for_gc_heap u64) => (return None);
            (@get is_subtype u32) => (return None);
            (@get ceil_f32 f32) => (return None);
            (@get ceil_f64 f64) => (return None);
            (@get floor_f32 f32) => (return None);
            (@get floor_f64 f64) => (return None);
            (@get trunc_f32 f32) => (return None);
            (@get trunc_f64 f64) => (return None);
            (@get nearest_f32 f32) => (return None);
            (@get nearest_f64 f64) => (return None);
            (@get i8x16_swizzle i8x16) => (return None);
            (@get i8x16_shuffle i8x16) => (return None);
            (@get fma_f32x4 f32x4) => (return None);
            (@get fma_f64x2 f64x2) => (return None);
            (@get passive_data_segment_base pointer) => (return None);
            (@get passive_elem_segment_len size) => (return None);
            (@get passive_elem_segment_base pointer) => (return None);

            (@get cont_new pointer) => (TrapSentinel::Negative);

            (@get get_instance_id u32) => (return None);

            // Bool-returning functions use `false` as an indicator of a trap.
            (@get $name:ident bool) => (TrapSentinel::Falsy);

            (@get $name:ident $ret:ident) => (
                compile_error!(concat!("no trap sentinel registered for ", stringify!($name)))
            )
        }

        foreach_builtin_function!(trap_sentinel)
    }
}

#[cfg(test)]
mod tests {
    use super::{BuiltinFunctionIndex, TrapSentinel};

    #[test]
    fn transaction_lifecycle_builtins_use_falsy_trap_sentinel() {
        for builtin in [
            BuiltinFunctionIndex::transaction_begin(),
            BuiltinFunctionIndex::transaction_commit(),
            BuiltinFunctionIndex::transaction_fail(),
        ] {
            assert!(matches!(builtin.trap_sentinel(), Some(TrapSentinel::Falsy)));
        }
    }

    #[test]
    fn transaction_data_builtins_use_expected_trap_sentinels() {
        for builtin in [
            BuiltinFunctionIndex::transaction_tglobal_set(),
            BuiltinFunctionIndex::transaction_tglobal_set_v128(),
            BuiltinFunctionIndex::transaction_ttable_set(),
            BuiltinFunctionIndex::transaction_ttable_read_range(),
            BuiltinFunctionIndex::transaction_ttable_write_range(),
            BuiltinFunctionIndex::transaction_tstruct_new(),
            BuiltinFunctionIndex::transaction_tstruct_set(),
        ] {
            assert!(matches!(builtin.trap_sentinel(), Some(TrapSentinel::Falsy)));
        }

        assert!(matches!(
            BuiltinFunctionIndex::transaction_tmemory_grow().trap_sentinel(),
            Some(TrapSentinel::NegativeTwo)
        ));
        assert!(matches!(
            BuiltinFunctionIndex::transaction_ttable_grow().trap_sentinel(),
            Some(TrapSentinel::NegativeTwo)
        ));

        assert!(matches!(
            BuiltinFunctionIndex::transaction_enter_tfunc().trap_sentinel(),
            Some(TrapSentinel::NegativeOne)
        ));

        for builtin in [
            BuiltinFunctionIndex::transaction_tglobal_get(),
            BuiltinFunctionIndex::transaction_tmemory_load(),
            BuiltinFunctionIndex::transaction_tmemory_store(),
            BuiltinFunctionIndex::transaction_tmemory_size(),
            BuiltinFunctionIndex::transaction_ttable_get(),
            BuiltinFunctionIndex::transaction_ttable_size(),
            BuiltinFunctionIndex::transaction_tstruct_get(),
        ] {
            assert!(matches!(
                builtin.trap_sentinel(),
                Some(TrapSentinel::NegativeOne)
            ));
        }
    }
}
