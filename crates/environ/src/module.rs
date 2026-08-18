//! Data structures for representing decoded wasm modules.

use crate::prelude::*;
use crate::*;
use core::ops::Range;
use cranelift_entity::{EntityRef, packed_option::ReservedValue};
use serde_derive::{Deserialize, Serialize};

/// A WebAssembly linear memory initializer.
#[derive(Clone, Debug)]
pub struct MemoryInitializer<'a> {
    /// The index of a linear memory to initialize.
    pub memory_index: MemoryIndex,
    /// The base offset to start this segment at.
    pub offset: ConstExpr,
    /// The range of the data to write within the linear memory.
    ///
    /// This range indexes into a separately stored data section which will be
    /// provided with the compiled module's code as well.
    pub data: &'a [u8],
}

/// A transactional WebAssembly linear-memory initializer.
#[derive(Clone, Debug)]
pub struct TMemoryInitializer<'a> {
    /// The transactional memory to initialize.
    pub memory_index: TMemoryIndex,
    /// The base offset to start this segment at.
    pub offset: ConstExpr,
    /// The bytes to write into the transactional memory.
    pub data: &'a [u8],
}

/// The type of WebAssembly linear memory initialization to use for a module.
#[derive(Debug, Serialize, Deserialize)]
pub enum MemoryInitialization {
    /// Memory initialization is segmented.
    ///
    /// Segmented initialization can be used for any module, but it is required
    /// if:
    ///
    /// * A data segment referenced an imported memory.
    /// * A data segment uses a global base.
    ///
    /// Segmented initialization is performed by processing the complete set of
    /// data segments when the module is instantiated.
    ///
    /// This is the default memory initialization type.
    Segmented,

    /// Memory initialization is statically known and involves a single `memcpy`
    /// or otherwise simply making the defined data visible.
    ///
    /// To be statically initialized everything must reference a defined memory
    /// and all data segments have a statically known in-bounds base (no
    /// globals).
    ///
    /// This form of memory initialization is a more optimized version of
    /// `Segmented` where memory can be initialized with one of a few methods:
    ///
    /// * First it could be initialized with a single `memcpy` of data from the
    ///   module to the linear memory.
    /// * Otherwise techniques like `mmap` are also possible to make this data,
    ///   which might reside in a compiled module on disk, available immediately
    ///   in a linear memory's address space.
    ///
    /// To facilitate the latter of these techniques the `try_static_init`
    /// function below, which creates this variant, takes a host page size
    /// argument which can page-align everything to make mmap-ing possible.
    Static {
        /// The initialization contents for each linear memory.
        ///
        /// This array has, for each module's own linear memory, the contents
        /// necessary to initialize it. If the memory has a `None` value then no
        /// initialization is necessary (it's zero-filled). Otherwise with
        /// `Some` the first element of the tuple is the offset in memory to
        /// start the initialization and the `Range` is the range within the
        /// final data section of the compiled module of bytes to copy into the
        /// memory.
        ///
        /// The offset, range base, and range end are all guaranteed to be page
        /// aligned to the page size passed in to `try_static_init`.
        map: TryPrimaryMap<MemoryIndex, Option<(u64, RuntimeDataIndex)>>,
    },
}

impl Default for MemoryInitialization {
    fn default() -> Self {
        Self::Segmented
    }
}

impl MemoryInitialization {
    /// Returns whether this initialization is of the form
    /// `MemoryInitialization::Segmented`.
    pub fn is_segmented(&self) -> bool {
        match self {
            MemoryInitialization::Segmented => true,
            _ => false,
        }
    }
}

/// Transactional linear-memory initialization for a module.
#[derive(Debug, Serialize, Deserialize)]
pub enum TMemoryInitialization {
    /// Initialization is performed from active segments at instantiation time.
    Segmented,
    /// Statically known initialization for each defined transactional memory.
    Static {
        /// Per-memory initialization offset and transactional runtime-data index.
        map: TryPrimaryMap<TMemoryIndex, Option<(u64, RuntimeTDataIndex)>>,
    },
}

impl Default for TMemoryInitialization {
    fn default() -> Self {
        Self::Segmented
    }
}

impl TMemoryInitialization {
    /// Returns whether this initialization uses active segments.
    pub fn is_segmented(&self) -> bool {
        matches!(self, Self::Segmented)
    }
}

/// Table initialization data for all tables in the module.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TableInitialization {
    /// Initial values for tables defined within the module itself.
    ///
    /// This contains the initial values and initializers for tables defined
    /// within a wasm, so excluding imported tables. This initializer can
    /// represent null-initialized tables, element-initialized tables (e.g. with
    /// the function-references proposal), or precomputed images of table
    /// initialization. For example table initializers to a table that are all
    /// in-bounds will get removed from `segment` and moved into
    /// `initial_values` here.
    pub initial_values: TryPrimaryMap<DefinedTableIndex, TableInitialValue>,

    /// Element segments present in the initial wasm module which are executed
    /// at instantiation time.
    ///
    /// These element segments are iterated over during instantiation to apply
    /// any segments that weren't already moved into `initial_values` above.
    pub segments: TryVec<TableSegment>,
}

/// Table initialization data for transactional tables in the module.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TTableInitialization {
    /// Initial values for defined transactional tables.
    pub initial_values: TryPrimaryMap<DefinedTTableIndex, TableInitialValue>,
    /// Active transactional element segments.
    pub segments: TryVec<TTableSegment>,
}

/// Initial value for all elements in a table.
#[derive(Debug, Serialize, Deserialize)]
pub enum TableInitialValue {
    /// Initialize each table element to null, optionally setting some elements
    /// to non-null given the precomputed image.
    Null,
    /// An arbitrary const expression.
    Expr(ConstExpr),
}

/// A WebAssembly table initializer segment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableSegment {
    /// The index of a table to initialize.
    pub table_index: TableIndex,
    /// The base offset to start this segment at.
    pub offset: ConstExpr,
    /// The values to write into the table elements.
    pub elements: TableSegmentElements,
}

/// A transactional WebAssembly table initializer segment.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TTableSegment {
    /// The transactional table to initialize.
    pub table_index: TTableIndex,
    /// The base offset to start this segment at.
    pub offset: ConstExpr,
    /// The values to write into the table elements.
    pub elements: TableSegmentElements,
}

/// Elements of a table segment, either a list of functions or list of arbitrary
/// expressions.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TableSegmentElements {
    /// A sequential list of functions where `FuncIndex::reserved_value()`
    /// indicates a null function.
    Functions(
        #[serde(deserialize_with = "crate::types::deserialize_boxed_slice")] Box<[FuncIndex]>,
    ),
    /// Arbitrary expressions, aka either functions, null or a load of a global.
    Expressions {
        /// The type of each element in `exprs`.
        ty: WasmRefType,
        /// The const expressions for this segment's elements.
        #[serde(deserialize_with = "crate::types::deserialize_boxed_slice")]
        exprs: Box<[ConstExpr]>,
    },
}

impl TableSegmentElements {
    /// Returns the type of this segment.
    pub fn ty(&self) -> WasmRefType {
        match self {
            Self::Functions(_) => WasmRefType::FUNCREF,
            Self::Expressions { ty, .. } => *ty,
        }
    }

    /// Returns the number of elements in this segment.
    pub fn len(&self) -> u64 {
        match self {
            Self::Functions(s) => u64::try_from(s.len()).unwrap(),
            Self::Expressions { exprs, .. } => u64::try_from(exprs.len()).unwrap(),
        }
    }
}

/// A translated WebAssembly module, excluding the function bodies and
/// memory initializers.
#[derive(Debug, Serialize, Deserialize)]
pub struct Module {
    /// This module's index.
    pub module_index: StaticModuleIndex,

    /// A pool of strings used in this module.
    pub strings: StringPool,

    /// The name of this wasm module, often found in the wasm file.
    pub name: Option<Atom>,

    /// All import records, in the order they are declared in the module.
    pub initializers: TryVec<Initializer>,

    /// Exported entities.
    pub exports: TryIndexMap<Atom, EntityIndex>,

    /// Whether or not this module has a start function,
    pub startup: ModuleStartup,

    /// Precompute per-table static images, if applicable.
    ///
    /// This map tracks, for each defined table in this module, the initial
    /// precomputed contents of the table. This is only applicable for funcref
    /// tables and the `TryVec` here uses `FuncIndex::reserved_value()` for null
    /// entries. This structure is filled in if table initialization is detected
    /// to be infallible as part of [`ModuleTranslation::finalize_table_init`].
    pub table_initialization: TryPrimaryMap<DefinedTableIndex, TryVec<FuncIndex>>,

    /// Precomputed initialization images for transactional tables.
    pub t_table_initialization: TryPrimaryMap<DefinedTTableIndex, TryVec<FuncIndex>>,

    /// WebAssembly linear memory initializer.
    ///
    /// This will track how memory is initialized, either exclusively via
    /// segments or if some memories can be initialized with static images. This
    /// is computed during [`ModuleTranslation::finalize_memory_init`].
    pub memory_initialization: MemoryInitialization,

    /// Transactional linear-memory initialization.
    pub t_memory_initialization: TMemoryInitialization,

    /// WebAssembly passive elements.
    ///
    /// This is a map of all passive element segments to their type and the
    /// initial size of the segment. Note that the contents of the segment are
    /// initialized by compiled code.
    pub passive_elements: TryPrimaryMap<PassiveElemIndex, (WasmRefType, u64)>,

    /// Passive transactional element segments.
    pub passive_telements: TryPrimaryMap<PassiveTElemIndex, (WasmRefType, u64)>,

    /// Where runtime data segments are located in the module's image.
    ///
    /// Note that this does not directly correspond to either active or passive
    /// data segments. Those are massaged during
    /// [`ModuleTranslation::finalize_memory_init`] into the form used here.
    pub runtime_data: TryPrimaryMap<RuntimeDataIndex, Range<u32>>,

    /// Transactional runtime data segments in the module image.
    pub runtime_tdata: TryPrimaryMap<RuntimeTDataIndex, Range<u32>>,

    /// Types declared in the wasm module.
    pub types: TryPrimaryMap<TypeIndex, EngineOrModuleTypeIndex>,

    /// Number of imported or aliased functions in the module.
    pub num_imported_funcs: usize,

    /// Number of imported or aliased tables in the module.
    pub num_imported_tables: usize,

    /// Number of imported transactional tables.
    pub num_imported_ttables: usize,

    /// Number of imported or aliased memories in the module.
    pub num_imported_memories: usize,

    /// Number of imported transactional memories.
    pub num_imported_tmemories: usize,

    /// Number of imported or aliased globals in the module.
    pub num_imported_globals: usize,

    /// Number of imported transactional globals.
    pub num_imported_tglobals: usize,

    /// Number of imported or aliased tags in the module.
    pub num_imported_tags: usize,

    /// Does this module need a GC heap to run?
    pub needs_gc_heap: bool,

    /// Number of functions that "escape" from this module may need to have a
    /// `VMFuncRef` constructed for them.
    ///
    /// This is also the number of functions in the `functions` array below with
    /// an `func_ref` index (and is the maximum func_ref index).
    pub num_escaped_funcs: usize,

    /// Types of functions, imported and local.
    pub functions: TryPrimaryMap<FuncIndex, FunctionType>,

    /// WebAssembly tables.
    pub tables: TryPrimaryMap<TableIndex, Table>,

    /// WebAssembly transactional tables.
    pub ttables: TryPrimaryMap<TTableIndex, Table>,

    /// WebAssembly linear memory plans.
    pub memories: TryPrimaryMap<MemoryIndex, Memory>,

    /// WebAssembly transactional linear memory plans.
    pub tmemories: TryPrimaryMap<TMemoryIndex, Memory>,

    /// WebAssembly global variables.
    pub globals: TryPrimaryMap<GlobalIndex, Global>,

    /// WebAssembly transactional global variables.
    pub tglobals: TryPrimaryMap<TGlobalIndex, Global>,

    /// "Simple" WebAssembly global initializers for locally-defined globals.
    ///
    /// This map does not track initialization of all globals in this module,
    /// but only those considered "simple" which can be easily evaluated at
    /// compile-time. For example an initialization expression of `i32.const N`
    /// is considered simple. These globals are manually initialized in the
    /// host.
    ///
    /// This is all in contrast to [`ModuleTranslation::global_initializers`]
    /// which is processed in compiled code and initialized after the instance
    /// has been created.
    pub global_initializers: TryVec<(DefinedGlobalIndex, GlobalConstValue)>,

    /// Simple initializers for defined transactional globals.
    pub tglobal_initializers: TryVec<(DefinedTGlobalIndex, GlobalConstValue)>,

    /// WebAssembly exception and control tags.
    pub tags: TryPrimaryMap<TagIndex, Tag>,
}

/// Initialization routines for creating an instance, encompassing imports,
/// modules, instances, aliases, etc.
#[derive(Debug, Serialize, Deserialize)]
pub enum Initializer {
    /// An imported item is required to be provided.
    Import {
        /// Name of this import
        name: Atom,
        /// The field name projection of this import
        field: Atom,
        /// Where this import will be placed, which also has type information
        /// about the import.
        index: EntityIndex,
    },
}

impl Module {
    /// Allocates the module data structures.
    pub fn new(module_index: StaticModuleIndex) -> Self {
        Self {
            module_index,
            strings: Default::default(),
            name: Default::default(),
            initializers: Default::default(),
            exports: Default::default(),
            startup: ModuleStartup::None,
            table_initialization: Default::default(),
            t_table_initialization: Default::default(),
            memory_initialization: Default::default(),
            t_memory_initialization: Default::default(),
            passive_elements: Default::default(),
            passive_telements: Default::default(),
            runtime_data: Default::default(),
            runtime_tdata: Default::default(),
            types: Default::default(),
            num_imported_funcs: Default::default(),
            num_imported_tables: Default::default(),
            num_imported_ttables: Default::default(),
            num_imported_memories: Default::default(),
            num_imported_tmemories: Default::default(),
            num_imported_globals: Default::default(),
            num_imported_tglobals: Default::default(),
            num_imported_tags: Default::default(),
            needs_gc_heap: Default::default(),
            num_escaped_funcs: Default::default(),
            functions: Default::default(),
            tables: Default::default(),
            ttables: Default::default(),
            memories: Default::default(),
            tmemories: Default::default(),
            globals: Default::default(),
            tglobals: Default::default(),
            global_initializers: Default::default(),
            tglobal_initializers: Default::default(),
            tags: Default::default(),
        }
    }

    /// Convert a `DefinedFuncIndex` into a `FuncIndex`.
    #[inline]
    pub fn func_index(&self, defined_func: DefinedFuncIndex) -> FuncIndex {
        FuncIndex::new(self.num_imported_funcs + defined_func.index())
    }

    /// Convert a `FuncIndex` into a `DefinedFuncIndex`. Returns None if the
    /// index is an imported function.
    #[inline]
    pub fn defined_func_index(&self, func: FuncIndex) -> Option<DefinedFuncIndex> {
        if func.index() < self.num_imported_funcs {
            None
        } else {
            Some(DefinedFuncIndex::new(
                func.index() - self.num_imported_funcs,
            ))
        }
    }

    /// Test whether the given function index is for an imported function.
    #[inline]
    pub fn is_imported_function(&self, index: FuncIndex) -> bool {
        index.index() < self.num_imported_funcs
    }

    /// Convert a `DefinedTableIndex` into a `TableIndex`.
    #[inline]
    pub fn table_index(&self, defined_table: DefinedTableIndex) -> TableIndex {
        TableIndex::new(self.num_imported_tables + defined_table.index())
    }

    /// Convert a `TableIndex` into a `DefinedTableIndex`. Returns None if the
    /// index is an imported table.
    #[inline]
    pub fn defined_table_index(&self, table: TableIndex) -> Option<DefinedTableIndex> {
        if table.index() < self.num_imported_tables {
            None
        } else {
            Some(DefinedTableIndex::new(
                table.index() - self.num_imported_tables,
            ))
        }
    }

    /// Test whether the given table index is for an exported table.
    #[inline]
    pub fn is_exported_table(&self, index: TableIndex) -> bool {
        self.exports.values().any(|entity| *entity == index.into())
    }

    /// Test whether the given table index is for an imported table.
    #[inline]
    pub fn is_imported_table(&self, index: TableIndex) -> bool {
        index.index() < self.num_imported_tables
    }

    /// Convert a defined transactional-table index into its module index.
    #[inline]
    pub fn ttable_index(&self, table: DefinedTTableIndex) -> TTableIndex {
        TTableIndex::new(self.num_imported_ttables + table.index())
    }

    /// Convert a transactional-table index into a defined index.
    #[inline]
    pub fn defined_ttable_index(&self, table: TTableIndex) -> Option<DefinedTTableIndex> {
        if table.index() < self.num_imported_ttables {
            None
        } else {
            Some(DefinedTTableIndex::new(
                table.index() - self.num_imported_ttables,
            ))
        }
    }

    /// Test whether the transactional table is imported.
    #[inline]
    pub fn is_imported_ttable(&self, table: TTableIndex) -> bool {
        table.index() < self.num_imported_ttables
    }

    /// Map a defined transactional table to its physical VMContext table slot.
    #[inline]
    pub fn runtime_defined_ttable_index(&self, table: DefinedTTableIndex) -> DefinedTableIndex {
        DefinedTableIndex::new(self.num_defined_tables() + table.index())
    }

    /// Recover a logical defined transactional table from a physical slot.
    pub fn defined_ttable_index_from_runtime(
        &self,
        table: DefinedTableIndex,
    ) -> Option<DefinedTTableIndex> {
        table
            .index()
            .checked_sub(self.num_defined_tables())
            .filter(|index| *index < self.num_defined_ttables())
            .map(DefinedTTableIndex::new)
    }

    /// Map an imported transactional table to its physical VMContext import slot.
    #[inline]
    pub fn runtime_imported_ttable_index(&self, table: TTableIndex) -> TableIndex {
        assert!(self.is_imported_ttable(table));
        TableIndex::new(self.num_imported_tables + table.index())
    }

    /// Convert a `DefinedMemoryIndex` into a `MemoryIndex`.
    #[inline]
    pub fn memory_index(&self, defined_memory: DefinedMemoryIndex) -> MemoryIndex {
        MemoryIndex::new(self.num_imported_memories + defined_memory.index())
    }

    /// Convert a `MemoryIndex` into a `DefinedMemoryIndex`. Returns None if the
    /// index is an imported memory.
    #[inline]
    pub fn defined_memory_index(&self, memory: MemoryIndex) -> Option<DefinedMemoryIndex> {
        if memory.index() < self.num_imported_memories {
            None
        } else {
            Some(DefinedMemoryIndex::new(
                memory.index() - self.num_imported_memories,
            ))
        }
    }

    /// Convert a `DefinedMemoryIndex` into an `OwnedMemoryIndex`. Returns None
    /// if the index is an imported memory.
    #[inline]
    pub fn owned_memory_index(&self, memory: DefinedMemoryIndex) -> OwnedMemoryIndex {
        assert!(
            memory.index() < self.memories.len(),
            "non-shared memory must have an owned index"
        );

        // Once we know that the memory index is not greater than the number of
        // plans, we can iterate through the plans up to the memory index and
        // count how many are not shared (i.e., owned).
        let owned_memory_index = self
            .memories
            .iter()
            .skip(self.num_imported_memories)
            .take(memory.index())
            .filter(|(_, mp)| !mp.shared)
            .count();
        OwnedMemoryIndex::new(owned_memory_index)
    }

    /// Test whether the given memory index is for an exported memory.
    #[inline]
    pub fn is_exported_memory(&self, index: MemoryIndex) -> bool {
        self.exports.values().any(|entity| *entity == index.into())
    }

    /// Test whether the given memory index is for an imported memory.
    #[inline]
    pub fn is_imported_memory(&self, index: MemoryIndex) -> bool {
        index.index() < self.num_imported_memories
    }

    /// Convert a defined transactional-memory index into its module index.
    #[inline]
    pub fn tmemory_index(&self, memory: DefinedTMemoryIndex) -> TMemoryIndex {
        TMemoryIndex::new(self.num_imported_tmemories + memory.index())
    }

    /// Convert a transactional-memory index into a defined index.
    #[inline]
    pub fn defined_tmemory_index(&self, memory: TMemoryIndex) -> Option<DefinedTMemoryIndex> {
        if memory.index() < self.num_imported_tmemories {
            None
        } else {
            Some(DefinedTMemoryIndex::new(
                memory.index() - self.num_imported_tmemories,
            ))
        }
    }

    /// Test whether the transactional memory is imported.
    #[inline]
    pub fn is_imported_tmemory(&self, memory: TMemoryIndex) -> bool {
        memory.index() < self.num_imported_tmemories
    }

    /// Map a defined transactional memory to its physical VMContext memory slot.
    #[inline]
    pub fn runtime_defined_tmemory_index(&self, memory: DefinedTMemoryIndex) -> DefinedMemoryIndex {
        DefinedMemoryIndex::new(self.num_defined_memories() + memory.index())
    }

    /// Recover a logical defined transactional memory from a physical slot.
    pub fn defined_tmemory_index_from_runtime(
        &self,
        memory: DefinedMemoryIndex,
    ) -> Option<DefinedTMemoryIndex> {
        memory
            .index()
            .checked_sub(self.num_defined_memories())
            .filter(|index| *index < self.num_defined_tmemories())
            .map(DefinedTMemoryIndex::new)
    }

    /// Map an imported transactional memory to its physical VMContext import slot.
    #[inline]
    pub fn runtime_imported_tmemory_index(&self, memory: TMemoryIndex) -> MemoryIndex {
        assert!(self.is_imported_tmemory(memory));
        MemoryIndex::new(self.num_imported_memories + memory.index())
    }

    /// Convert a `DefinedGlobalIndex` into a `GlobalIndex`.
    #[inline]
    pub fn global_index(&self, defined_global: DefinedGlobalIndex) -> GlobalIndex {
        GlobalIndex::new(self.num_imported_globals + defined_global.index())
    }

    /// Convert a `GlobalIndex` into a `DefinedGlobalIndex`. Returns None if the
    /// index is an imported global.
    #[inline]
    pub fn defined_global_index(&self, global: GlobalIndex) -> Option<DefinedGlobalIndex> {
        if global.index() < self.num_imported_globals {
            None
        } else {
            Some(DefinedGlobalIndex::new(
                global.index() - self.num_imported_globals,
            ))
        }
    }

    /// Test whether the given global index is for an exported global.
    #[inline]
    pub fn is_exported_global(&self, index: GlobalIndex) -> bool {
        self.exports.values().any(|entity| *entity == index.into())
    }

    /// Test whether the given global index is for an imported global.
    #[inline]
    pub fn is_imported_global(&self, index: GlobalIndex) -> bool {
        index.index() < self.num_imported_globals
    }

    /// Convert a defined transactional-global index into its module index.
    #[inline]
    pub fn tglobal_index(&self, global: DefinedTGlobalIndex) -> TGlobalIndex {
        TGlobalIndex::new(self.num_imported_tglobals + global.index())
    }

    /// Convert a transactional-global index into a defined index.
    #[inline]
    pub fn defined_tglobal_index(&self, global: TGlobalIndex) -> Option<DefinedTGlobalIndex> {
        if global.index() < self.num_imported_tglobals {
            None
        } else {
            Some(DefinedTGlobalIndex::new(
                global.index() - self.num_imported_tglobals,
            ))
        }
    }

    /// Test whether the transactional global is imported.
    #[inline]
    pub fn is_imported_tglobal(&self, global: TGlobalIndex) -> bool {
        global.index() < self.num_imported_tglobals
    }

    /// Map a defined transactional global to its physical VMContext global slot.
    #[inline]
    pub fn runtime_defined_tglobal_index(&self, global: DefinedTGlobalIndex) -> DefinedGlobalIndex {
        DefinedGlobalIndex::new(self.num_defined_globals() + global.index())
    }

    /// Recover a logical defined transactional global from a physical slot.
    pub fn defined_tglobal_index_from_runtime(
        &self,
        global: DefinedGlobalIndex,
    ) -> Option<DefinedTGlobalIndex> {
        global
            .index()
            .checked_sub(self.num_defined_globals())
            .filter(|index| *index < self.num_defined_tglobals())
            .map(DefinedTGlobalIndex::new)
    }

    /// Map an imported transactional global to its physical VMContext import slot.
    #[inline]
    pub fn runtime_imported_tglobal_index(&self, global: TGlobalIndex) -> GlobalIndex {
        assert!(self.is_imported_tglobal(global));
        GlobalIndex::new(self.num_imported_globals + global.index())
    }

    /// Number of physical imported table slots, across both namespaces.
    pub fn num_runtime_imported_tables(&self) -> usize {
        self.num_imported_tables + self.num_imported_ttables
    }

    /// Number of physical imported memory slots, across both namespaces.
    pub fn num_runtime_imported_memories(&self) -> usize {
        self.num_imported_memories + self.num_imported_tmemories
    }

    /// Number of physical imported global slots, across both namespaces.
    pub fn num_runtime_imported_globals(&self) -> usize {
        self.num_imported_globals + self.num_imported_tglobals
    }

    /// Number of physical defined table slots, across both namespaces.
    pub fn num_runtime_defined_tables(&self) -> usize {
        self.num_defined_tables() + self.num_defined_ttables()
    }

    /// Number of physical defined memory slots, across both namespaces.
    pub fn num_runtime_defined_memories(&self) -> usize {
        self.num_defined_memories() + self.num_defined_tmemories()
    }

    /// Number of physical defined global slots, across both namespaces.
    pub fn num_runtime_defined_globals(&self) -> usize {
        self.num_defined_globals() + self.num_defined_tglobals()
    }

    /// Map transactional runtime data to its physical VMContext runtime-data slot.
    pub fn runtime_tdata_index(&self, data: RuntimeTDataIndex) -> RuntimeDataIndex {
        RuntimeDataIndex::new(self.runtime_data.len() + data.index())
    }

    /// Test whether the given tag index is for an imported tag.
    #[inline]
    pub fn is_imported_tag(&self, index: TagIndex) -> bool {
        index.index() < self.num_imported_tags
    }

    /// Convert a `DefinedTagIndex` into a `TagIndex`.
    #[inline]
    pub fn tag_index(&self, defined_tag: DefinedTagIndex) -> TagIndex {
        TagIndex::new(self.num_imported_tags + defined_tag.index())
    }

    /// Convert a `TagIndex` into a `DefinedTagIndex`. Returns None if the
    /// index is an imported tag.
    #[inline]
    pub fn defined_tag_index(&self, tag: TagIndex) -> Option<DefinedTagIndex> {
        if tag.index() < self.num_imported_tags {
            None
        } else {
            Some(DefinedTagIndex::new(tag.index() - self.num_imported_tags))
        }
    }

    /// Returns an iterator of all the imports in this module, along with their
    /// module name, field name, and type that's being imported.
    pub fn imports(&self) -> impl ExactSizeIterator<Item = (&str, &str, EntityType)> {
        let pool = &self.strings;
        self.initializers.iter().map(move |i| match i {
            Initializer::Import { name, field, index } => {
                (&pool[name], &pool[field], self.type_of(*index))
            }
        })
    }

    /// Get this module's `i`th import.
    pub fn import(&self, i: usize) -> Option<(&str, &str, EntityType)> {
        match self.initializers.get(i)? {
            Initializer::Import { name, field, index } => Some((
                &self.strings[name],
                &self.strings[field],
                self.type_of(*index),
            )),
        }
    }

    /// Returns the type of an item based on its index
    pub fn type_of(&self, index: EntityIndex) -> EntityType {
        match index {
            EntityIndex::Global(i) => EntityType::Global(self.globals[i]),
            EntityIndex::TGlobal(i) => EntityType::TGlobal(self.tglobals[i]),
            EntityIndex::Table(i) => EntityType::Table(self.tables[i]),
            EntityIndex::TTable(i) => EntityType::TTable(self.ttables[i]),
            EntityIndex::Memory(i) => EntityType::Memory(self.memories[i]),
            EntityIndex::TMemory(i) => EntityType::TMemory(self.tmemories[i]),
            EntityIndex::Function(i) => EntityType::Function(self.functions[i].signature),
            EntityIndex::Tag(i) => EntityType::Tag(self.tags[i]),
        }
    }

    /// Appends a new tag to this module with the given type information.
    pub fn push_tag(
        &mut self,
        signature: impl Into<EngineOrModuleTypeIndex>,
        exception: impl Into<EngineOrModuleTypeIndex>,
    ) -> TagIndex {
        let signature = signature.into();
        let exception = exception.into();
        self.tags
            .push(Tag {
                signature,
                exception,
            })
            .panic_on_oom()
    }

    /// Appends a new function to this module with the given type information,
    /// used for functions that either don't escape or aren't certain whether
    /// they escape yet.
    pub fn push_function(&mut self, signature: impl Into<EngineOrModuleTypeIndex>) -> FuncIndex {
        self.push_function_with_transaction(signature, false)
    }

    /// Append a function and retain whether its signature is transactional.
    pub fn push_function_with_transaction(
        &mut self,
        signature: impl Into<EngineOrModuleTypeIndex>,
        is_transactional: bool,
    ) -> FuncIndex {
        let signature = signature.into();
        self.functions
            .push(FunctionType {
                signature,
                func_ref: FuncRefIndex::reserved_value(),
                is_transactional,
            })
            .panic_on_oom()
    }

    /// Returns whether a function has a native transactional function type.
    pub fn is_tfunc(&self, func: FuncIndex) -> bool {
        self.functions[func].is_transactional
    }

    /// Returns an iterator over all of the defined function indices in this
    /// module.
    pub fn defined_func_indices(&self) -> impl ExactSizeIterator<Item = DefinedFuncIndex> + use<> {
        (0..self.functions.len() - self.num_imported_funcs).map(|i| DefinedFuncIndex::new(i))
    }

    /// Returns the number of functions defined by this module itself: all
    /// functions minus imported functions.
    pub fn num_defined_funcs(&self) -> usize {
        self.functions.len() - self.num_imported_funcs
    }

    /// Returns the number of tables defined by this module itself: all tables
    /// minus imported tables.
    pub fn num_defined_tables(&self) -> usize {
        self.tables.len() - self.num_imported_tables
    }

    /// Number of defined transactional tables.
    pub fn num_defined_ttables(&self) -> usize {
        self.ttables.len() - self.num_imported_ttables
    }

    /// Returns the number of memories defined by this module itself: all
    /// memories minus imported memories.
    pub fn num_defined_memories(&self) -> usize {
        self.memories.len() - self.num_imported_memories
    }

    /// Number of defined transactional memories.
    pub fn num_defined_tmemories(&self) -> usize {
        self.tmemories.len() - self.num_imported_tmemories
    }

    /// Returns the number of globals defined by this module itself: all
    /// globals minus imported globals.
    pub fn num_defined_globals(&self) -> usize {
        self.globals.len() - self.num_imported_globals
    }

    /// Number of defined transactional globals.
    pub fn num_defined_tglobals(&self) -> usize {
        self.tglobals.len() - self.num_imported_tglobals
    }

    /// Returns the number of tags defined by this module itself: all tags
    /// minus imported tags.
    pub fn num_defined_tags(&self) -> usize {
        self.tags.len() - self.num_imported_tags
    }

    /// Tests whether `index` is valid for this module.
    pub fn is_valid(&self, index: EntityIndex) -> bool {
        match index {
            EntityIndex::Function(i) => self.functions.is_valid(i),
            EntityIndex::Table(i) => self.tables.is_valid(i),
            EntityIndex::TTable(i) => self.ttables.is_valid(i),
            EntityIndex::Memory(i) => self.memories.is_valid(i),
            EntityIndex::TMemory(i) => self.tmemories.is_valid(i),
            EntityIndex::Global(i) => self.globals.is_valid(i),
            EntityIndex::TGlobal(i) => self.tglobals.is_valid(i),
            EntityIndex::Tag(i) => self.tags.is_valid(i),
        }
    }
}

impl TypeTrace for Module {
    fn trace<F, E>(&self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        // NB: Do not `..` elide unmodified fields so that we get compile errors
        // when adding new fields that might need re-canonicalization.
        let Self {
            module_index: _,
            strings: _,
            name: _,
            initializers: _,
            exports: _,
            startup,
            table_initialization: _,
            t_table_initialization: _,
            memory_initialization: _,
            t_memory_initialization: _,
            passive_elements: _,
            passive_telements: _,
            runtime_data: _,
            runtime_tdata: _,
            types,
            num_imported_funcs: _,
            num_imported_tables: _,
            num_imported_ttables: _,
            num_imported_memories: _,
            num_imported_tmemories: _,
            num_imported_globals: _,
            num_imported_tglobals: _,
            num_imported_tags: _,
            num_escaped_funcs: _,
            needs_gc_heap: _,
            functions,
            tables,
            ttables,
            memories: _,
            tmemories: _,
            globals,
            tglobals,
            global_initializers: _,
            tglobal_initializers: _,
            tags,
        } = self;

        for t in types.values().copied() {
            func(t)?;
        }
        for f in functions.values() {
            f.trace(func)?;
        }
        for t in tables.values() {
            t.trace(func)?;
        }
        for t in ttables.values() {
            t.trace(func)?;
        }
        for g in globals.values() {
            g.trace(func)?;
        }
        for g in tglobals.values() {
            g.trace(func)?;
        }
        for t in tags.values() {
            t.trace(func)?;
        }
        startup.trace(func)?;
        Ok(())
    }

    fn trace_mut<F, E>(&mut self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(&mut EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        // NB: Do not `..` elide unmodified fields so that we get compile errors
        // when adding new fields that might need re-canonicalization.
        let Self {
            module_index: _,
            strings: _,
            name: _,
            initializers: _,
            exports: _,
            startup,
            table_initialization: _,
            t_table_initialization: _,
            memory_initialization: _,
            t_memory_initialization: _,
            passive_elements: _,
            passive_telements: _,
            runtime_data: _,
            runtime_tdata: _,
            types,
            num_imported_funcs: _,
            num_imported_tables: _,
            num_imported_ttables: _,
            num_imported_memories: _,
            num_imported_tmemories: _,
            num_imported_globals: _,
            num_imported_tglobals: _,
            num_imported_tags: _,
            num_escaped_funcs: _,
            needs_gc_heap: _,
            functions,
            tables,
            ttables,
            memories: _,
            tmemories: _,
            globals,
            tglobals,
            global_initializers: _,
            tglobal_initializers: _,
            tags,
        } = self;

        for t in types.values_mut() {
            func(t)?;
        }
        for f in functions.values_mut() {
            f.trace_mut(func)?;
        }
        for t in tables.values_mut() {
            t.trace_mut(func)?;
        }
        for t in ttables.values_mut() {
            t.trace_mut(func)?;
        }
        for g in globals.values_mut() {
            g.trace_mut(func)?;
        }
        for g in tglobals.values_mut() {
            g.trace_mut(func)?;
        }
        for t in tags.values_mut() {
            t.trace_mut(func)?;
        }
        startup.trace_mut(func)?;
        Ok(())
    }
}

impl TypeTrace for ModuleStartup {
    fn trace<F, E>(&self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        match self {
            ModuleStartup::None => Ok(()),
            ModuleStartup::Always(t) | ModuleStartup::IfMemoriesNeedInit(t) => func(*t),
        }
    }

    fn trace_mut<F, E>(&mut self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(&mut EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        match self {
            ModuleStartup::None => Ok(()),
            ModuleStartup::Always(t) | ModuleStartup::IfMemoriesNeedInit(t) => func(t),
        }
    }
}

/// Type information about functions in a wasm module.
#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionType {
    /// The type of this function, indexed into the module-wide type tables for
    /// a module compilation.
    pub signature: EngineOrModuleTypeIndex,
    /// The index into the funcref table, if present. Note that this is
    /// `reserved_value()` if the function does not escape from a module.
    pub func_ref: FuncRefIndex,
    /// Whether this function's type was encoded as a transactional function.
    pub is_transactional: bool,
}

impl TypeTrace for FunctionType {
    fn trace<F, E>(&self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        func(self.signature)
    }

    fn trace_mut<F, E>(&mut self, func: &mut F) -> Result<(), E>
    where
        F: FnMut(&mut EngineOrModuleTypeIndex) -> Result<(), E>,
    {
        func(&mut self.signature)
    }
}

impl FunctionType {
    /// Returns whether this function's type is one that "escapes" the current
    /// module, meaning that the function is exported, used in `ref.func`, used
    /// in a table, etc.
    pub fn is_escaping(&self) -> bool {
        !self.func_ref.is_reserved_value()
    }
}

/// Index into the funcref table within a VMContext for a function.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct FuncRefIndex(u32);
cranelift_entity::entity_impl!(FuncRefIndex);

/// Different means of startup for a wasm module.
#[derive(Debug, Serialize, Deserialize)]
pub enum ModuleStartup {
    /// No startup is necessary.
    None,

    /// Startup is always required, for example to apply active table segments.
    ///
    /// The type of the startup function, of wasm signature `[] -> []`, is
    /// provided here.
    Always(EngineOrModuleTypeIndex),

    /// Startup is only required if some linear memory within this module, at
    /// runtime, says `needs_init() == true`.
    ///
    /// This special mode of startup indicates that the startup function has no
    /// purpose other than to initialize the initial contents of
    /// `MemoryInitialization::Static` linear memories. In this situation if all
    /// memories say `needs_init() == false` then the startup function won't
    /// actually do anything meaning that it can be optimized slightly by
    /// skipping it entirely.
    IfMemoriesNeedInit(EngineOrModuleTypeIndex),
}

impl ModuleStartup {
    /// Returns if this is `ModuleStartup::None`.
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}
