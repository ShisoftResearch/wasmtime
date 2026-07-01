use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use wasm_encoder::reencode::{Error as ReencodeError, Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, ConstExpr, Encode, Function, GlobalType as EncoderGlobalType,
    HeapType as EncoderHeapType, ImportSection, Instruction, Module, RawSection,
    RefType as EncoderRefType, TransactionRefPermission as EncoderTransactionRefPermission,
    ValType as EncoderValType,
};
use wasmparser::{
    AbstractHeapType, BinaryReader, BlockType, CodeSectionReader, CompositeInnerType, DataKind,
    ExternalKind, HeapType, KnownCustom, Name, Operator, Parser, Payload, StorageType, TypeRef,
    ValType as ParserValType,
};

use super::metadata::{
    KotlinCopyableType, KotlinField, KotlinFieldKind, KotlinGcWasmCapture, KotlinPersistentKind,
    KotlinPersistentType, KotlinSidecar, validate_kotlin_sidecar,
};

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";
const TRANSACTION_OBJECTS_VERSION: u8 = 1;
const KOTLIN_ROOT_GET_IMPORT_MODULE: &str = "twasm.root.get";
const KOTLIN_ROOT_SET_IMPORT_MODULE: &str = "twasm.root.set";
const NAME_CUSTOM_SECTION: &str = "name";
const KOTLIN_RUNTIME_STRUCT_FIELDS: &[&str] = &["vtable", "itable", "rtti", "_hashCode"];
const KOTLIN_INLINE_ROOT_MARKER: &str = "twasm root marker was not lowered:";
const KOTLIN_INLINE_SET_ROOT_MARKER: &str = "twasm setRoot marker was not lowered:";

#[derive(Debug, Default, Serialize)]
pub struct KotlinRewriteReport {
    pub transaction_functions: Vec<String>,
    pub persistent_types: usize,
    pub roots: usize,
    pub rewritten_tfuncs: usize,
    pub rewritten_object_ops: usize,
}

#[derive(Default)]
struct PersistentTypeIndices {
    structs: BTreeSet<u32>,
    arrays: BTreeSet<u32>,
}

#[derive(Default)]
struct GcTypeInfo {
    persistent: PersistentTypeIndices,
    explicit_persistent: PersistentTypeIndices,
    copyable: PersistentTypeIndices,
    module_type_indices: BTreeMap<String, u32>,
    copyable_type_indices: BTreeMap<String, u32>,
    denied_type_indices: BTreeSet<u32>,
    sidecar_type_indices: BTreeMap<String, u32>,
    struct_field_counts: BTreeMap<u32, usize>,
    struct_field_names: BTreeMap<(u32, String), BTreeSet<u32>>,
    struct_field_storage: BTreeMap<(u32, u32), StorageType>,
    array_element_storage: BTreeMap<u32, StorageType>,
    struct_fields: BTreeMap<(u32, u32), ParserValType>,
    array_elements: BTreeMap<u32, ParserValType>,
    globals: BTreeMap<u32, ParserValType>,
    explicit_persistent_struct_fields: BTreeMap<(u32, u32), KotlinField>,
    explicit_persistent_array_elements: BTreeMap<u32, KotlinField>,
    copyable_struct_fields: BTreeMap<(u32, u32), KotlinField>,
    copyable_array_elements: BTreeMap<u32, KotlinField>,
}

#[derive(Default)]
struct RequiredTemps {
    needs_i32: bool,
    values: BTreeSet<ParserValType>,
}

#[derive(Default)]
struct TempLocals {
    i32: Option<u32>,
    values: BTreeMap<ParserValType, u32>,
}

#[derive(Default)]
struct TransactionObjects {
    memories: BTreeSet<u32>,
    globals: BTreeSet<u32>,
    functions: BTreeSet<u32>,
    tables: BTreeSet<u32>,
}

#[derive(Clone)]
struct RootGlobal {
    name: String,
    type_index: u32,
    global_index: u32,
}

struct RootLowering {
    globals: Vec<RootGlobal>,
    unit_getter_func: Option<u32>,
    get_imports: BTreeMap<u32, RootGlobal>,
    set_imports: BTreeMap<u32, RootGlobal>,
}

#[derive(Clone)]
struct FunctionSignature {
    params: Vec<ParserValType>,
    results: Vec<ParserValType>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefProvenance {
    NonRef,
    NullRef,
    OrdinaryRef,
    PersistentRef,
    TxnRefAllowed,
    TxnLocalCopyable,
    UnknownRef,
}

impl RefProvenance {
    fn can_enter_persistent_value(self) -> bool {
        matches!(
            self,
            Self::NullRef | Self::PersistentRef | Self::TxnRefAllowed | Self::TxnLocalCopyable
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StackValue {
    type_index: Option<u32>,
    provenance: RefProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProvenanceControlKind {
    Block,
    Loop,
    If,
}

struct ProvenanceControlFrame {
    kind: ProvenanceControlKind,
    entry_locals: Vec<StackValue>,
    then_locals: Option<Vec<StackValue>>,
}

impl StackValue {
    fn non_ref() -> Self {
        Self {
            type_index: None,
            provenance: RefProvenance::NonRef,
        }
    }

    fn ref_value(type_index: Option<u32>, provenance: RefProvenance) -> Self {
        Self {
            type_index,
            provenance,
        }
    }
}

struct KotlinIndexRemapper {
    function_index_map: Vec<Option<u32>>,
}

#[derive(Debug)]
struct KotlinRemapError(String);

impl fmt::Display for KotlinRemapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for KotlinRemapError {}

impl KotlinIndexRemapper {
    fn new(function_index_map: Vec<Option<u32>>) -> Self {
        Self { function_index_map }
    }

    fn remap_function_index(&self, function_index: u32) -> Result<u32> {
        self.function_index_map
            .get(function_index as usize)
            .copied()
            .flatten()
            .with_context(|| format!("function index {function_index} was removed during rewrite"))
    }

    fn remap_function_index_for_reencode(
        &self,
        function_index: u32,
    ) -> std::result::Result<u32, ReencodeError<KotlinRemapError>> {
        self.function_index_map
            .get(function_index as usize)
            .copied()
            .flatten()
            .ok_or_else(|| {
                ReencodeError::UserError(KotlinRemapError(format!(
                    "function index {function_index} was removed during rewrite"
                )))
            })
    }

    fn function_index_removed(&self, function_index: u32) -> bool {
        self.function_index_map
            .get(function_index as usize)
            .is_some_and(Option::is_none)
    }
}

impl Reencode for KotlinIndexRemapper {
    type Error = KotlinRemapError;

    fn function_index(
        &mut self,
        function_index: u32,
    ) -> std::result::Result<u32, ReencodeError<Self::Error>> {
        self.remap_function_index_for_reencode(function_index)
    }
}

impl TransactionObjects {
    fn is_empty(&self) -> bool {
        self.memories.is_empty()
            && self.globals.is_empty()
            && self.functions.is_empty()
            && self.tables.is_empty()
    }
}

impl RootLowering {
    fn marker_import_indices(&self) -> BTreeSet<u32> {
        self.get_imports
            .keys()
            .chain(self.set_imports.keys())
            .copied()
            .collect()
    }
}

fn root_lowering(
    input: &[u8],
    sidecar: &KotlinSidecar,
    gc_type_info: &GcTypeInfo,
) -> Result<RootLowering> {
    let imported_globals = imported_global_count(input)?;
    let defined_globals = defined_global_count(input)?;
    let first_root_global = imported_globals
        .checked_add(defined_globals)
        .context("Kotlin root global index overflow")?;
    let mut globals = Vec::new();

    for (ordinal, root) in sidecar.roots.iter().enumerate() {
        let type_index = *gc_type_info
            .sidecar_type_indices
            .get(&root.r#type)
            .with_context(|| {
                format!(
                    "Kotlin root {} has unmapped type {}",
                    root.name, root.r#type
                )
            })?;
        let ordinal = u32::try_from(ordinal).context("Kotlin root ordinal overflow")?;
        let global_index = first_root_global
            .checked_add(ordinal)
            .context("Kotlin root global index overflow")?;
        globals.push(RootGlobal {
            name: root.name.clone(),
            type_index,
            global_index,
        });
    }

    // SHISOFT-TWASM-MOCK: current inline setRoot lowering reconstructs Kotlin
    // Unit through this exact frontend helper name. Replace this with an
    // explicit SDK intrinsic once the Kotlin transaction frontend is stable.
    let unit_getter_func = function_index_by_exact_name(input, "kotlin.Unit_getInstance")?;
    let unit_getter_result = unit_getter_func
        .map(|function_index| {
            function_signature(input, function_index).and_then(|signature| {
                let signature = signature.with_context(|| {
                    format!("missing Kotlin Unit getter function index {function_index}")
                })?;
                ensure!(
                    signature.params.is_empty(),
                    "kotlin.Unit_getInstance must not have parameters"
                );
                ensure!(
                    signature.results.len() == 1,
                    "kotlin.Unit_getInstance must return one value"
                );
                Ok(signature.results[0])
            })
        })
        .transpose()?;
    let (get_imports, set_imports) =
        explicit_root_marker_imports(input, &globals, unit_getter_result)?;

    Ok(RootLowering {
        globals,
        unit_getter_func,
        get_imports,
        set_imports,
    })
}

fn append_root_globals(globals: &mut wasm_encoder::GlobalSection, root_lowering: &RootLowering) {
    for root in &root_lowering.globals {
        globals.global(
            EncoderGlobalType {
                val_type: EncoderValType::Ref(EncoderRefType {
                    nullable: true,
                    heap_type: EncoderHeapType::Concrete(root.type_index),
                    transaction_permission: EncoderTransactionRefPermission::None,
                }),
                mutable: true,
                shared: false,
            },
            &ConstExpr::ref_null(EncoderHeapType::Concrete(root.type_index)),
        );
    }
}

fn emit_root_global_section_if_needed(
    module: &mut Module,
    root_lowering: &RootLowering,
    emitted_global_section: &mut bool,
) {
    if *emitted_global_section || root_lowering.globals.is_empty() {
        return;
    }

    let mut globals = wasm_encoder::GlobalSection::new();
    append_root_globals(&mut globals, root_lowering);
    module.section(&globals);
    *emitted_global_section = true;
}

pub fn rewrite_kotlin_module(
    input: &[u8],
    sidecar: &KotlinSidecar,
) -> Result<(Vec<u8>, KotlinRewriteReport)> {
    validate_kotlin_sidecar(sidecar)?;

    let transaction_function_indices = transaction_function_indices(input, sidecar)?;
    let imported_function_count = imported_function_count(input)?;
    let marker_literals = KotlinMarkerLiterals::new(input)?;
    let transaction_call_closure_function_indices = transaction_call_closure_function_indices(
        input,
        &transaction_function_indices,
        imported_function_count,
    )?;
    let ordinary_shared_function_indices = ordinary_shared_function_indices(input)?;
    let transaction_call_closure_function_indices = transaction_call_closure_function_indices
        .into_iter()
        .filter(|index| {
            transaction_function_indices.contains(index)
                || !ordinary_shared_function_indices.contains(index)
        })
        .collect::<BTreeSet<_>>();
    let persistent_accessor_function_indices =
        persistent_accessor_function_indices(input, sidecar)?;
    let mut object_rewrite_function_indices = transaction_call_closure_function_indices.clone();
    object_rewrite_function_indices.extend(persistent_accessor_function_indices.iter().copied());
    let txref_get_function_indices = txref_get_function_indices(input)?;
    for index in &txref_get_function_indices {
        object_rewrite_function_indices.remove(index);
    }
    let mut transaction_objects = transaction_objects(input)?;
    let gc_type_info = gc_type_info(input, sidecar)?;
    let root_lowering = root_lowering(input, sidecar, &gc_type_info)?;
    let stripped_marker_imports = root_lowering.marker_import_indices();
    let mut index_remapper =
        KotlinIndexRemapper::new(function_index_map(input, &stripped_marker_imports)?);
    remap_transaction_objects(&mut transaction_objects, &index_remapper)?;
    transaction_objects.functions.extend(
        transaction_call_closure_function_indices
            .iter()
            .filter(|index| !persistent_accessor_function_indices.contains(index))
            .map(|index| index_remapper.remap_function_index(*index))
            .collect::<Result<Vec<_>>>()?,
    );
    transaction_objects
        .globals
        .extend(root_lowering.globals.iter().map(|root| root.global_index));
    let func_type_params = function_type_params(input)?;
    let function_signatures = function_signatures_by_index(input)?;
    let defined_function_types = defined_function_type_indices(input)?;
    let root_marker_function_indices = root_marker_function_indices(
        input,
        &root_lowering,
        imported_function_count,
        &func_type_params,
        &defined_function_types,
        &marker_literals,
    )?;
    let root_publisher_function_indices = root_marker_function_indices
        .iter()
        .filter_map(|(index, uses)| (uses.set && !uses.get).then_some(*index))
        .collect::<BTreeSet<_>>();
    for index in &root_publisher_function_indices {
        object_rewrite_function_indices.remove(index);
    }
    transaction_objects.functions.extend(
        root_marker_function_indices
            .keys()
            .map(|index| index_remapper.remap_function_index(*index))
            .collect::<Result<Vec<_>>>()?,
    );

    let mut report = KotlinRewriteReport {
        transaction_functions: sidecar.transaction_functions.clone(),
        persistent_types: gc_type_info.persistent.structs.len()
            + gc_type_info.persistent.arrays.len(),
        roots: sidecar.roots.len(),
        rewritten_tfuncs: transaction_function_indices.len(),
        rewritten_object_ops: 0,
    };

    let mut module = Module::new();
    let mut next_defined_func = 0u32;
    let mut emitted_global_section = false;

    for payload in Parser::new(0).parse_all(input) {
        let payload = payload.context("failed to parse Kotlin rewrite wasm payload")?;
        match payload {
            Payload::Version {
                encoding: wasmparser::Encoding::Module,
                ..
            } => {}
            Payload::Version { .. } => bail!("unsupported non-core wasm module"),
            Payload::TypeSection(section) => {
                let mut types = wasm_encoder::TypeSection::new();
                index_remapper.parse_type_section(&mut types, section)?;
                module.section(&types);
            }
            Payload::ImportSection(section) => {
                let mut imports = ImportSection::new();
                rewrite_kotlin_import_section(&mut index_remapper, &mut imports, section)?;
                if !imports.is_empty() {
                    module.section(&imports);
                }
            }
            Payload::FunctionSection(section) => {
                let mut functions = wasm_encoder::FunctionSection::new();
                index_remapper.parse_function_section(&mut functions, section)?;
                module.section(&functions);
            }
            Payload::TableSection(section) => {
                let mut tables = wasm_encoder::TableSection::new();
                index_remapper.parse_table_section(&mut tables, section)?;
                module.section(&tables);
            }
            Payload::MemorySection(section) => {
                let mut memories = wasm_encoder::MemorySection::new();
                index_remapper.parse_memory_section(&mut memories, section)?;
                module.section(&memories);
            }
            Payload::TagSection(section) => {
                let mut tags = wasm_encoder::TagSection::new();
                index_remapper.parse_tag_section(&mut tags, section)?;
                module.section(&tags);
            }
            Payload::GlobalSection(section) => {
                let mut globals = wasm_encoder::GlobalSection::new();
                index_remapper.parse_global_section(&mut globals, section)?;
                append_root_globals(&mut globals, &root_lowering);
                module.section(&globals);
                emitted_global_section = true;
            }
            Payload::ExportSection(section) => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let mut exports = wasm_encoder::ExportSection::new();
                index_remapper.parse_export_section(&mut exports, section)?;
                module.section(&exports);
            }
            Payload::StartSection { func, .. } => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                module.section(&wasm_encoder::StartSection {
                    function_index: index_remapper.start_section(func)?,
                });
            }
            Payload::ElementSection(section) => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let mut elements = wasm_encoder::ElementSection::new();
                index_remapper.parse_element_section(&mut elements, section)?;
                module.section(&elements);
            }
            Payload::DataCountSection { count, .. } => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                module.section(&wasm_encoder::DataCountSection {
                    count: index_remapper.data_count(count)?,
                });
            }
            Payload::DataSection(section) => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let mut data = wasm_encoder::DataSection::new();
                index_remapper.parse_data_section(&mut data, section)?;
                module.section(&data);
            }
            Payload::CodeSectionStart { range, .. } => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid Kotlin rewrite code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;
                let mut code = CodeSection::new();

                for body in section {
                    let body = body?;
                    let function_index = imported_function_count + next_defined_func;
                    let type_index = *defined_function_types
                        .get(next_defined_func as usize)
                        .context("missing Kotlin rewrite function type index")?;
                    let params = func_type_params
                        .get(&type_index)
                        .cloned()
                        .context("missing Kotlin rewrite function type parameters")?;
                    let function = rewrite_function_body(
                        &body,
                        object_rewrite_function_indices.contains(&function_index),
                        &gc_type_info,
                        &root_lowering,
                        &mut index_remapper,
                        &params,
                        &function_signatures,
                        &txref_get_function_indices,
                        &marker_literals,
                        &mut report,
                    )?;
                    code.function(&function);
                    next_defined_func += 1;
                }

                module.section(&code);
            }
            Payload::CodeSectionEntry(_) => {}
            Payload::CustomSection(section) => {
                if section.name() != TRANSACTION_OBJECTS_CUSTOM_SECTION
                    && !(section.name() == NAME_CUSTOM_SECTION
                        && !stripped_marker_imports.is_empty())
                {
                    module.section(&wasm_encoder::CustomSection::from(section));
                }
            }
            Payload::End(_) => {}
            other => {
                let (id, range) = other
                    .as_section()
                    .context("unsupported non-section Kotlin rewrite payload")?;
                let contents = input
                    .get(range.start..range.end)
                    .context("invalid Kotlin rewrite section range")?;
                module.section(&RawSection { id, data: contents });
            }
        }
    }

    if !transaction_objects.is_empty() {
        module.section(&wasm_encoder::CustomSection {
            name: Cow::Borrowed(TRANSACTION_OBJECTS_CUSTOM_SECTION),
            data: Cow::Owned(encode_transaction_objects(&transaction_objects)),
        });
    }

    Ok((module.finish(), report))
}

fn rewrite_function_body(
    body: &wasmparser::FunctionBody<'_>,
    rewrite_object_ops: bool,
    gc_type_info: &GcTypeInfo,
    root_lowering: &RootLowering,
    index_remapper: &mut KotlinIndexRemapper,
    params: &[ParserValType],
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    txref_get_function_indices: &BTreeSet<u32>,
    marker_literals: &KotlinMarkerLiterals,
    report: &mut KotlinRewriteReport,
) -> Result<Function> {
    let mut type_reencoder = RoundtripReencoder;
    let local_types = local_types(body, params)?;
    let required_temps = required_temps(body, rewrite_object_ops, gc_type_info)?;
    let (local_decls, temp_locals) =
        local_decls_with_temps(&mut type_reencoder, body, &local_types, required_temps)?;
    let mut function = Function::new(local_decls);
    let mut value_stack = Vec::new();
    let mut local_values = local_types
        .iter()
        .copied()
        .map(|ty| stack_value_for_type(ty, gc_type_info))
        .collect::<Vec<_>>();
    let mut control_frames = Vec::new();
    let mut reader = body.get_operators_reader()?;
    let mut operators = Vec::new();

    while !reader.eof() {
        operators.push(reader.read()?);
    }

    let mut index = 0usize;
    while index < operators.len() {
        if let Some(marker) =
            match_inline_set_root_marker(&operators, index, &local_types, marker_literals)?
        {
            if let Some(root) = root_for_marker_type(root_lowering, marker.type_index)? {
                let value = local_values
                    .get(marker.value_local as usize)
                    .copied()
                    .unwrap_or_else(StackValue::non_ref);
                validate_root_value_boundary(root, value, gc_type_info)?;
                let unit_getter = root_lowering
                    .unit_getter_func
                    .context("Kotlin setRoot marker lowering requires kotlin.Unit_getInstance")?;
                let unit_getter = index_remapper.remap_function_index(unit_getter)?;
                function.instruction(&Instruction::LocalGet(marker.value_local));
                function.instruction(&Instruction::TGlobalSet {
                    global_index: root.global_index,
                });
                function.instruction(&Instruction::Call(unit_getter));
                value_stack.push(StackValue::non_ref());
                index = marker.next_index;
                continue;
            }
        }

        if let Some(marker) = match_inline_get_root_marker(&operators, index, marker_literals)?
            && let Some(root) = root_for_marker_type(root_lowering, marker.type_index)?
        {
            function.instruction(&Instruction::TGlobalGet {
                global_index: root.global_index,
            });
            value_stack.push(StackValue::ref_value(
                Some(root.type_index),
                RefProvenance::PersistentRef,
            ));
            index = marker.next_index;
            continue;
        }

        let op = operators[index].clone();
        if let Operator::Call { function_index } = op {
            if let Some(root) = root_lowering.get_imports.get(&function_index) {
                function.instruction(&Instruction::TGlobalGet {
                    global_index: root.global_index,
                });
                value_stack.push(StackValue::ref_value(
                    Some(root.type_index),
                    RefProvenance::PersistentRef,
                ));
                index += 1;
                continue;
            }
            if let Some(root) = root_lowering.set_imports.get(&function_index) {
                let value = value_stack.pop().unwrap_or_else(StackValue::non_ref);
                validate_root_value_boundary(root, value, gc_type_info)?;
                if let Some(type_index) = value.type_index
                    && type_index != root.type_index
                {
                    bail!(
                        "Kotlin setRoot marker for root {} consumed type index {type_index}, expected {}",
                        root.name,
                        root.type_index
                    );
                }
                let unit_getter = root_lowering
                    .unit_getter_func
                    .context("Kotlin setRoot marker lowering requires kotlin.Unit_getInstance")?;
                let unit_getter = index_remapper.remap_function_index(unit_getter)?;
                function.instruction(&Instruction::TGlobalSet {
                    global_index: root.global_index,
                });
                function.instruction(&Instruction::Call(unit_getter));
                value_stack.push(StackValue::non_ref());
                index += 1;
                continue;
            }
            if txref_get_function_indices.contains(&function_index) {
                update_stack_for_call(
                    function_signatures.get(&function_index),
                    gc_type_info,
                    &mut value_stack,
                    RefProvenance::TxnRefAllowed,
                );
                function
                    .instruction(&index_remapper.instruction(Operator::Call { function_index })?);
                index += 1;
                continue;
            }
        }
        let is_array_len = matches!(op, Operator::ArrayLen);
        let array_len_operand = if is_array_len {
            value_stack.pop().and_then(|value| value.type_index)
        } else {
            if rewrite_object_ops {
                validate_persistent_boundaries_for_operator(&op, gc_type_info, &value_stack)?;
            }
            update_value_stack_for_operator(
                &op,
                &local_types,
                &mut local_values,
                &mut control_frames,
                function_signatures,
                gc_type_info,
                &mut value_stack,
            );
            None
        };
        match op {
            Operator::StructGet {
                struct_type_index,
                field_index,
            } if rewrite_object_ops
                && gc_type_info.persistent.structs.contains(&struct_type_index) =>
            {
                function.instruction(&Instruction::TRefCastRead);
                function.instruction(&Instruction::TStructGet {
                    struct_type_index,
                    field_index,
                });
                report.rewritten_object_ops += 1;
            }
            Operator::StructGetS {
                struct_type_index,
                field_index,
            } if rewrite_object_ops
                && gc_type_info.persistent.structs.contains(&struct_type_index) =>
            {
                function.instruction(&Instruction::TRefCastRead);
                function.instruction(&Instruction::TStructGetS {
                    struct_type_index,
                    field_index,
                });
                report.rewritten_object_ops += 1;
            }
            Operator::StructGetU {
                struct_type_index,
                field_index,
            } if rewrite_object_ops
                && gc_type_info.persistent.structs.contains(&struct_type_index) =>
            {
                function.instruction(&Instruction::TRefCastRead);
                function.instruction(&Instruction::TStructGetU {
                    struct_type_index,
                    field_index,
                });
                report.rewritten_object_ops += 1;
            }
            Operator::StructSet {
                struct_type_index,
                field_index,
            } if rewrite_object_ops
                && gc_type_info.persistent.structs.contains(&struct_type_index) =>
            {
                let field_ty = *gc_type_info
                    .struct_fields
                    .get(&(struct_type_index, field_index))
                    .context("missing Kotlin rewrite struct field type")?;
                let value_temp = temp_locals
                    .values
                    .get(&field_ty)
                    .copied()
                    .context("missing Kotlin rewrite struct field temp local")?;
                function.instruction(&Instruction::LocalSet(value_temp));
                function.instruction(&Instruction::TRefCastWrite);
                function.instruction(&Instruction::LocalGet(value_temp));
                function.instruction(&Instruction::TStructSet {
                    struct_type_index,
                    field_index,
                });
                report.rewritten_object_ops += 1;
            }
            Operator::ArrayGet { array_type_index }
                if rewrite_object_ops
                    && gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                emit_array_read_prefix(&mut function, &temp_locals)?;
                function.instruction(&Instruction::TArrayGet(array_type_index));
                report.rewritten_object_ops += 1;
            }
            Operator::ArrayGetS { array_type_index }
                if rewrite_object_ops
                    && gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                emit_array_read_prefix(&mut function, &temp_locals)?;
                function.instruction(&Instruction::TArrayGetS(array_type_index));
                report.rewritten_object_ops += 1;
            }
            Operator::ArrayGetU { array_type_index }
                if rewrite_object_ops
                    && gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                emit_array_read_prefix(&mut function, &temp_locals)?;
                function.instruction(&Instruction::TArrayGetU(array_type_index));
                report.rewritten_object_ops += 1;
            }
            Operator::ArraySet { array_type_index }
                if rewrite_object_ops
                    && gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                let element_ty = *gc_type_info
                    .array_elements
                    .get(&array_type_index)
                    .context("missing Kotlin rewrite array element type")?;
                let value_temp = temp_locals
                    .values
                    .get(&element_ty)
                    .copied()
                    .context("missing Kotlin rewrite array element temp local")?;
                let index_temp = temp_locals
                    .i32
                    .context("missing Kotlin rewrite array index temp local")?;
                function.instruction(&Instruction::LocalSet(value_temp));
                function.instruction(&Instruction::LocalSet(index_temp));
                function.instruction(&Instruction::TRefCastWrite);
                function.instruction(&Instruction::LocalGet(index_temp));
                function.instruction(&Instruction::LocalGet(value_temp));
                function.instruction(&Instruction::TArraySet(array_type_index));
                report.rewritten_object_ops += 1;
            }
            Operator::ArrayLen
                if rewrite_object_ops
                    && array_len_operand.is_some_and(|type_index| {
                        gc_type_info.persistent.arrays.contains(&type_index)
                    }) =>
            {
                function.instruction(&Instruction::TArrayLen);
                report.rewritten_object_ops += 1;
                value_stack.push(StackValue::non_ref());
            }
            _ => {
                function.instruction(&index_remapper.instruction(op)?);
                if is_array_len {
                    value_stack.push(StackValue::non_ref());
                }
            }
        }
        index += 1;
    }

    Ok(function)
}

fn local_types(
    body: &wasmparser::FunctionBody<'_>,
    params: &[ParserValType],
) -> Result<Vec<ParserValType>> {
    let mut locals = params.to_vec();
    for local in body.get_locals_reader()? {
        let (count, ty) = local.context("failed to parse Kotlin rewrite local")?;
        for _ in 0..count {
            locals.push(ty);
        }
    }
    Ok(locals)
}

fn required_temps(
    body: &wasmparser::FunctionBody<'_>,
    rewrite_object_ops: bool,
    gc_type_info: &GcTypeInfo,
) -> Result<RequiredTemps> {
    let mut temps = RequiredTemps::default();
    if !rewrite_object_ops {
        return Ok(temps);
    }

    let mut reader = body.get_operators_reader()?;
    while !reader.eof() {
        match reader.read()? {
            Operator::StructSet {
                struct_type_index,
                field_index,
            } if gc_type_info.persistent.structs.contains(&struct_type_index) => {
                temps.values.insert(
                    *gc_type_info
                        .struct_fields
                        .get(&(struct_type_index, field_index))
                        .context("missing Kotlin rewrite struct field type")?,
                );
            }
            Operator::ArrayGet { array_type_index }
            | Operator::ArrayGetS { array_type_index }
            | Operator::ArrayGetU { array_type_index }
                if gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                temps.needs_i32 = true;
            }
            Operator::ArraySet { array_type_index }
                if gc_type_info.persistent.arrays.contains(&array_type_index) =>
            {
                temps.needs_i32 = true;
                temps.values.insert(
                    *gc_type_info
                        .array_elements
                        .get(&array_type_index)
                        .context("missing Kotlin rewrite array element type")?,
                );
            }
            _ => {}
        }
    }

    Ok(temps)
}

fn local_decls_with_temps(
    reencoder: &mut RoundtripReencoder,
    body: &wasmparser::FunctionBody<'_>,
    local_types: &[ParserValType],
    required_temps: RequiredTemps,
) -> Result<(Vec<(u32, EncoderValType)>, TempLocals)> {
    let mut local_decls = Vec::new();
    for local in body.get_locals_reader()? {
        let (count, ty) = local.context("failed to parse Kotlin rewrite local")?;
        local_decls.push((count, reencoder.val_type(ty)?));
    }

    let mut next_local = local_types.len() as u32;
    let mut temps = TempLocals::default();

    if required_temps.needs_i32 {
        temps.i32 = Some(next_local);
        next_local += 1;
        local_decls.push((1, EncoderValType::I32));
    }

    for ty in required_temps.values {
        temps.values.insert(ty, next_local);
        next_local += 1;
        local_decls.push((1, reencoder.val_type(ty)?));
    }

    Ok((local_decls, temps))
}

fn emit_array_read_prefix(function: &mut Function, temp_locals: &TempLocals) -> Result<()> {
    let index_temp = temp_locals
        .i32
        .context("missing Kotlin rewrite array index temp local")?;
    function.instruction(&Instruction::LocalSet(index_temp));
    function.instruction(&Instruction::TRefCastRead);
    function.instruction(&Instruction::LocalGet(index_temp));
    Ok(())
}

struct SetRootMarker {
    next_index: usize,
    type_index: u32,
    value_local: u32,
}

struct GetRootMarker {
    next_index: usize,
    type_index: u32,
}

struct KotlinMarkerLiterals {
    root: BTreeSet<i32>,
    set_root: BTreeSet<i32>,
}

impl KotlinMarkerLiterals {
    fn new(input: &[u8]) -> Result<Self> {
        let mut root = BTreeSet::from([659]);
        let mut set_root = BTreeSet::from([658]);

        for (index, literal) in kotlin_latin1_string_pool(input)? {
            if literal.starts_with(KOTLIN_INLINE_ROOT_MARKER) {
                root.insert(index);
            }
            if literal.starts_with(KOTLIN_INLINE_SET_ROOT_MARKER) {
                set_root.insert(index);
            }
        }

        Ok(Self { root, set_root })
    }
}

fn kotlin_latin1_string_pool(input: &[u8]) -> Result<Vec<(i32, String)>> {
    let mut data_segments = Vec::new();
    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin string-pool payload")? {
            Payload::DataSection(section) => {
                for data in section {
                    let data = data.context("failed to parse Kotlin data segment")?;
                    if matches!(data.kind, DataKind::Passive) {
                        data_segments.push(data.data.to_vec());
                    }
                }
            }
            _ => {}
        }
    }

    if data_segments.len() < 2 {
        return Ok(Vec::new());
    }

    let strings = &data_segments[0];
    let address_and_lengths = &data_segments[1];
    let mut literals = Vec::new();
    for (index, slot) in address_and_lengths.chunks_exact(8).enumerate() {
        let encoded = u64::from_le_bytes(slot.try_into().unwrap());
        let start = (encoded & 0xffff_ffff) as usize;
        let len = (encoded >> 32) as usize;
        let Some(end) = start.checked_add(len) else {
            continue;
        };
        let Some(bytes) = strings.get(start..end) else {
            continue;
        };
        let literal = bytes
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>();
        let Ok(index) = i32::try_from(index) else {
            break;
        };
        literals.push((index, literal));
    }

    Ok(literals)
}

// SHISOFT-TWASM-MOCK: this recognizes the current Kotlin SDK inline
// root/setRoot throw-marker shape. Replace it with explicit SDK imports or
// compiler-emitted intrinsics once the Kotlin frontend contract is stabilized.
fn match_inline_set_root_marker(
    operators: &[Operator<'_>],
    index: usize,
    local_types: &[ParserValType],
    marker_literals: &KotlinMarkerLiterals,
) -> Result<Option<SetRootMarker>> {
    if !matches!(
        operators.get(index),
        Some(Operator::Block {
            blockty: BlockType::Type(_)
        })
    ) {
        return Ok(None);
    }
    let Some(end) = matching_block_end(operators, index) else {
        return Ok(None);
    };
    let body = &operators[index + 1..end];
    if !body_contains_i32_const_in(body, &marker_literals.set_root)
        || !body_ends_with_throw_unreachable(body)
    {
        return Ok(None);
    }

    let marker = body.windows(3).find_map(|window| match window {
        [
            Operator::LocalGet {
                local_index: source,
            },
            Operator::LocalSet {
                local_index: target,
            },
            Operator::I32Const { value },
        ] => {
            if !marker_literals.set_root.contains(value) {
                return None;
            }
            let source_type = local_type_index(local_types, *source)?;
            (local_type_index(local_types, *target) == Some(source_type)).then_some(SetRootMarker {
                next_index: end + 1,
                type_index: source_type,
                value_local: *source,
            })
        }
        _ => None,
    });

    Ok(marker)
}

fn match_inline_get_root_marker(
    operators: &[Operator<'_>],
    index: usize,
    marker_literals: &KotlinMarkerLiterals,
) -> Result<Option<GetRootMarker>> {
    let type_index = match operators.get(index) {
        Some(Operator::Block {
            blockty: BlockType::Type(ty),
        }) => module_ref_type_index(*ty),
        _ => None,
    };
    let Some(type_index) = type_index else {
        return Ok(None);
    };
    let Some(end) = matching_block_end(operators, index) else {
        return Ok(None);
    };
    let body = &operators[index + 1..end];
    if body_contains_i32_const_in(body, &marker_literals.root)
        && body_ends_with_throw_unreachable(body)
    {
        Ok(Some(GetRootMarker {
            next_index: end + 1,
            type_index,
        }))
    } else {
        Ok(None)
    }
}

fn root_for_marker_type<'a>(
    root_lowering: &'a RootLowering,
    type_index: u32,
) -> Result<Option<&'a RootGlobal>> {
    // SHISOFT-TWASM-MOCK: inline Kotlin root markers currently expose the root
    // result/value type but not a stable decoded root-name operand. Distinct
    // root types can be resolved; same-type roots fail loudly below.
    let matches = root_lowering
        .globals
        .iter()
        .filter(|root| root.type_index == type_index)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [root] => Ok(Some(*root)),
        _ => {
            let names = matches
                .iter()
                .map(|root| root.name.as_str())
                .collect::<Vec<_>>();
            bail!(
                "ambiguous Kotlin root marker for type index {type_index}: roots {names:?}; use distinct root types or add explicit root marker imports"
            )
        }
    }
}

fn body_contains_i32_const_in(operators: &[Operator<'_>], values: &BTreeSet<i32>) -> bool {
    operators
        .iter()
        .any(|op| matches!(op, Operator::I32Const { value } if values.contains(value)))
}

fn body_ends_with_throw_unreachable(operators: &[Operator<'_>]) -> bool {
    operators.windows(3).any(|window| {
        matches!(
            window,
            [Operator::Throw { .. }, Operator::End, Operator::Unreachable]
        )
    })
}

fn matching_block_end(operators: &[Operator<'_>], start: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, op) in operators.iter().enumerate().skip(start + 1) {
        match op {
            Operator::Block { .. }
            | Operator::Loop { .. }
            | Operator::If { .. }
            | Operator::TryTable { .. }
            | Operator::Try { .. } => depth += 1,
            Operator::End => {
                if depth == 0 {
                    return Some(offset);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

fn validate_persistent_boundaries_for_operator(
    op: &Operator<'_>,
    gc_type_info: &GcTypeInfo,
    value_stack: &[StackValue],
) -> Result<()> {
    match op {
        Operator::StructNew { struct_type_index } => {
            validate_struct_constructor_boundary(*struct_type_index, gc_type_info, value_stack)
        }
        Operator::StructSet {
            struct_type_index,
            field_index,
        } => validate_struct_field_value_boundary(
            *struct_type_index,
            *field_index,
            value_stack
                .last()
                .copied()
                .unwrap_or_else(StackValue::non_ref),
            gc_type_info,
        ),
        Operator::ArrayNew { array_type_index } => {
            let value = value_stack
                .iter()
                .rev()
                .nth(1)
                .copied()
                .unwrap_or_else(StackValue::non_ref);
            validate_array_element_value_boundary(*array_type_index, value, gc_type_info)
        }
        Operator::ArrayNewFixed {
            array_type_index,
            array_size,
        } => {
            if !array_requires_source_boundary(*array_type_index, gc_type_info) {
                return Ok(());
            }
            let array_size =
                usize::try_from(*array_size).context("Kotlin rewrite array size overflow")?;
            for value in value_stack.iter().rev().take(array_size).copied() {
                validate_array_element_value_boundary(*array_type_index, value, gc_type_info)?;
            }
            Ok(())
        }
        Operator::ArraySet { array_type_index } => {
            let value = value_stack
                .last()
                .copied()
                .unwrap_or_else(StackValue::non_ref);
            validate_array_element_value_boundary(*array_type_index, value, gc_type_info)
        }
        Operator::ArrayFill { array_type_index } => {
            let value = value_stack
                .iter()
                .rev()
                .nth(1)
                .copied()
                .unwrap_or_else(StackValue::non_ref);
            validate_array_element_value_boundary(*array_type_index, value, gc_type_info)
        }
        _ => Ok(()),
    }
}

fn update_value_stack_for_operator(
    op: &Operator<'_>,
    local_types: &[ParserValType],
    local_values: &mut Vec<StackValue>,
    control_frames: &mut Vec<ProvenanceControlFrame>,
    function_signatures: &BTreeMap<u32, FunctionSignature>,
    gc_type_info: &GcTypeInfo,
    value_stack: &mut Vec<StackValue>,
) {
    match op {
        Operator::Block { .. } => {
            control_frames.push(ProvenanceControlFrame {
                kind: ProvenanceControlKind::Block,
                entry_locals: local_values.clone(),
                then_locals: None,
            });
            value_stack.clear();
        }
        Operator::Loop { .. } => {
            control_frames.push(ProvenanceControlFrame {
                kind: ProvenanceControlKind::Loop,
                entry_locals: local_values.clone(),
                then_locals: None,
            });
            value_stack.clear();
        }
        Operator::If { .. } => {
            value_stack.pop();
            control_frames.push(ProvenanceControlFrame {
                kind: ProvenanceControlKind::If,
                entry_locals: local_values.clone(),
                then_locals: None,
            });
            value_stack.clear();
        }
        Operator::Else => {
            value_stack.clear();
            if let Some(frame) = control_frames.last_mut()
                && frame.kind == ProvenanceControlKind::If
            {
                frame.then_locals = Some(local_values.clone());
                *local_values = frame.entry_locals.clone();
            } else {
                invalidate_ref_locals(local_values, local_types, gc_type_info);
            }
        }
        Operator::End => {
            value_stack.clear();
            if let Some(frame) = control_frames.pop()
                && frame.kind == ProvenanceControlKind::If
            {
                let current_locals = local_values.clone();
                let merged = if let Some(then_locals) = frame.then_locals {
                    merge_local_values(&then_locals, &current_locals, local_types, gc_type_info)
                } else {
                    merge_local_values(
                        &frame.entry_locals,
                        &current_locals,
                        local_types,
                        gc_type_info,
                    )
                };
                *local_values = merged;
            }
        }
        Operator::LocalGet { local_index } => {
            let value = local_values
                .get(*local_index as usize)
                .copied()
                .unwrap_or_else(StackValue::non_ref);
            value_stack.push(value);
        }
        Operator::Drop => {
            value_stack.pop();
        }
        Operator::LocalSet { local_index } => {
            let value = value_stack.pop().unwrap_or_else(StackValue::non_ref);
            ensure_local_value_capacity(local_values, local_types, *local_index, gc_type_info);
            if let Some(local) = local_values.get_mut(*local_index as usize) {
                *local = value;
            }
        }
        Operator::LocalTee { local_index } => {
            let value = value_stack
                .last()
                .copied()
                .unwrap_or_else(StackValue::non_ref);
            ensure_local_value_capacity(local_values, local_types, *local_index, gc_type_info);
            if let Some(local) = local_values.get_mut(*local_index as usize) {
                *local = value;
            }
        }
        Operator::GlobalGet { global_index } => {
            let value = gc_type_info
                .globals
                .get(global_index)
                .copied()
                .map(|ty| stack_value_for_type(ty, gc_type_info))
                .unwrap_or_else(|| StackValue::ref_value(None, RefProvenance::UnknownRef));
            value_stack.push(value);
        }
        Operator::GlobalSet { .. } => {
            value_stack.pop();
        }
        Operator::I32Const { .. }
        | Operator::I64Const { .. }
        | Operator::F32Const { .. }
        | Operator::F64Const { .. }
        | Operator::V128Const { .. } => {
            value_stack.push(StackValue::non_ref());
        }
        Operator::StructGet { .. } | Operator::StructGetS { .. } | Operator::StructGetU { .. } => {
            let (struct_type_index, field_index) = match op {
                Operator::StructGet {
                    struct_type_index,
                    field_index,
                }
                | Operator::StructGetS {
                    struct_type_index,
                    field_index,
                }
                | Operator::StructGetU {
                    struct_type_index,
                    field_index,
                } => (*struct_type_index, *field_index),
                _ => unreachable!(),
            };
            let receiver = value_stack.pop().unwrap_or_else(StackValue::non_ref);
            let field_ty = gc_type_info
                .struct_fields
                .get(&(struct_type_index, field_index))
                .copied();
            value_stack.push(stack_value_for_read_field(field_ty, receiver, gc_type_info));
        }
        Operator::StructSet { .. } => {
            pop_n(value_stack, 2);
        }
        Operator::StructNew { struct_type_index } => {
            let count = gc_type_info
                .struct_field_counts
                .get(struct_type_index)
                .copied()
                .unwrap_or(0);
            pop_n(value_stack, count);
            value_stack.push(stack_value_for_constructor(
                *struct_type_index,
                gc_type_info,
            ));
        }
        Operator::StructNewDefault { struct_type_index } => {
            value_stack.push(stack_value_for_constructor(
                *struct_type_index,
                gc_type_info,
            ));
        }
        Operator::ArrayGet { .. } | Operator::ArrayGetS { .. } | Operator::ArrayGetU { .. } => {
            let array_type_index = match op {
                Operator::ArrayGet { array_type_index }
                | Operator::ArrayGetS { array_type_index }
                | Operator::ArrayGetU { array_type_index } => *array_type_index,
                _ => unreachable!(),
            };
            let _index = value_stack.pop();
            let receiver = value_stack.pop().unwrap_or_else(StackValue::non_ref);
            let element_ty = gc_type_info.array_elements.get(&array_type_index).copied();
            value_stack.push(stack_value_for_read_field(
                element_ty,
                receiver,
                gc_type_info,
            ));
        }
        Operator::ArraySet { .. } => {
            pop_n(value_stack, 3);
        }
        Operator::ArrayNew { array_type_index } => {
            pop_n(value_stack, 2);
            value_stack.push(stack_value_for_constructor(*array_type_index, gc_type_info));
        }
        Operator::ArrayNewDefault { array_type_index } => {
            value_stack.pop();
            value_stack.push(stack_value_for_constructor(*array_type_index, gc_type_info));
        }
        Operator::ArrayNewFixed {
            array_type_index,
            array_size,
        } => {
            pop_n(
                value_stack,
                usize::try_from(*array_size).unwrap_or(usize::MAX),
            );
            value_stack.push(stack_value_for_constructor(*array_type_index, gc_type_info));
        }
        Operator::ArrayNewData {
            array_type_index, ..
        }
        | Operator::ArrayNewElem {
            array_type_index, ..
        } => {
            pop_n(value_stack, 2);
            value_stack.push(stack_value_for_constructor(*array_type_index, gc_type_info));
        }
        Operator::ArrayFill { .. } => {
            pop_n(value_stack, 4);
        }
        Operator::RefNull { hty } => {
            value_stack.push(StackValue::ref_value(
                heap_type_index(*hty),
                RefProvenance::NullRef,
            ));
        }
        Operator::RefAsNonNull => {}
        Operator::Call { function_index } => {
            update_stack_for_call(
                function_signatures.get(function_index),
                gc_type_info,
                value_stack,
                RefProvenance::UnknownRef,
            );
        }
        Operator::Br { .. }
        | Operator::BrIf { .. }
        | Operator::BrTable { .. }
        | Operator::Return
        | Operator::ReturnCall { .. }
        | Operator::ReturnCallIndirect { .. }
        | Operator::Unreachable
        | Operator::Throw { .. }
        | Operator::Rethrow { .. }
        | Operator::ThrowRef => {
            invalidate_ref_locals(local_values, local_types, gc_type_info);
            value_stack.clear();
        }
        _ => {
            value_stack.clear();
        }
    }
}

fn pop_n<T>(stack: &mut Vec<T>, count: usize) {
    for _ in 0..count {
        stack.pop();
    }
}

fn ensure_local_value_capacity(
    local_values: &mut Vec<StackValue>,
    local_types: &[ParserValType],
    local_index: u32,
    gc_type_info: &GcTypeInfo,
) {
    while local_values.len() <= local_index as usize {
        let value = local_types
            .get(local_values.len())
            .copied()
            .map(|ty| stack_value_for_type(ty, gc_type_info))
            .unwrap_or_else(StackValue::non_ref);
        local_values.push(value);
    }
}

fn merge_local_values(
    left: &[StackValue],
    right: &[StackValue],
    local_types: &[ParserValType],
    gc_type_info: &GcTypeInfo,
) -> Vec<StackValue> {
    let len = left.len().max(right.len()).max(local_types.len());
    (0..len)
        .map(|index| {
            let fallback = local_types
                .get(index)
                .copied()
                .map(|ty| stack_value_for_type(ty, gc_type_info))
                .unwrap_or_else(StackValue::non_ref);
            let left = left.get(index).copied().unwrap_or(fallback);
            let right = right.get(index).copied().unwrap_or(fallback);
            merge_stack_value(left, right, fallback)
        })
        .collect()
}

fn merge_stack_value(left: StackValue, right: StackValue, fallback: StackValue) -> StackValue {
    if left == right {
        return left;
    }
    if fallback.provenance == RefProvenance::NonRef {
        return StackValue::non_ref();
    }
    StackValue::ref_value(
        fallback.type_index.or(left.type_index).or(right.type_index),
        RefProvenance::UnknownRef,
    )
}

fn invalidate_ref_locals(
    local_values: &mut [StackValue],
    local_types: &[ParserValType],
    gc_type_info: &GcTypeInfo,
) {
    for (index, local) in local_values.iter_mut().enumerate() {
        let fallback = local_types
            .get(index)
            .copied()
            .map(|ty| stack_value_for_type(ty, gc_type_info))
            .unwrap_or_else(StackValue::non_ref);
        if fallback.provenance != RefProvenance::NonRef || local.provenance != RefProvenance::NonRef
        {
            *local = StackValue::ref_value(
                fallback.type_index.or(local.type_index),
                RefProvenance::UnknownRef,
            );
        }
    }
}

fn update_stack_for_call(
    signature: Option<&FunctionSignature>,
    gc_type_info: &GcTypeInfo,
    value_stack: &mut Vec<StackValue>,
    ref_result_provenance: RefProvenance,
) {
    let Some(signature) = signature else {
        value_stack.clear();
        return;
    };
    pop_n(value_stack, signature.params.len());
    for result in &signature.results {
        let mut value = stack_value_for_type(*result, gc_type_info);
        if value.provenance != RefProvenance::NonRef {
            value.provenance = ref_result_provenance;
        }
        value_stack.push(value);
    }
}

fn validate_root_value_boundary(
    root: &RootGlobal,
    value: StackValue,
    gc_type_info: &GcTypeInfo,
) -> Result<()> {
    validate_ref_value_boundary(
        &format!("persistent root {}", root.name),
        value,
        Some(root.type_index),
        gc_type_info,
    )
}

fn validate_struct_constructor_boundary(
    struct_type_index: u32,
    gc_type_info: &GcTypeInfo,
    value_stack: &[StackValue],
) -> Result<()> {
    if !struct_requires_source_boundary(struct_type_index, gc_type_info) {
        return Ok(());
    }
    let fields = sidecar_struct_fields_for_boundary(struct_type_index, gc_type_info);
    if fields.is_empty() {
        return Ok(());
    }
    let count = *gc_type_info
        .struct_field_counts
        .get(&struct_type_index)
        .context("missing Kotlin rewrite struct field count")?;
    let values = value_stack.iter().rev().take(count).collect::<Vec<_>>();
    for (field_index, field) in fields {
        let offset = count
            .checked_sub(1 + field_index as usize)
            .context("Kotlin rewrite struct field index overflow")?;
        let value = values
            .get(offset)
            .copied()
            .copied()
            .unwrap_or_else(StackValue::non_ref);
        if field.kind == KotlinFieldKind::Ref {
            validate_ref_value_boundary(
                &field_boundary_context(struct_type_index, field_index, field, gc_type_info),
                value,
                expected_type_index_for_field(field, gc_type_info),
                gc_type_info,
            )?;
        }
    }
    Ok(())
}

fn validate_struct_field_value_boundary(
    struct_type_index: u32,
    field_index: u32,
    value: StackValue,
    gc_type_info: &GcTypeInfo,
) -> Result<()> {
    let Some(field) =
        sidecar_struct_field_for_boundary(struct_type_index, field_index, gc_type_info)
    else {
        return Ok(());
    };
    if field.kind != KotlinFieldKind::Ref {
        return Ok(());
    }
    validate_ref_value_boundary(
        &field_boundary_context(struct_type_index, field_index, field, gc_type_info),
        value,
        expected_type_index_for_field(field, gc_type_info),
        gc_type_info,
    )
}

fn validate_array_element_value_boundary(
    array_type_index: u32,
    value: StackValue,
    gc_type_info: &GcTypeInfo,
) -> Result<()> {
    let Some(element) = sidecar_array_element_for_boundary(array_type_index, gc_type_info) else {
        return Ok(());
    };
    if element.kind != KotlinFieldKind::Ref {
        return Ok(());
    }
    validate_ref_value_boundary(
        &format!(
            "persistent array {} element",
            type_name_for_index(array_type_index, gc_type_info)
                .unwrap_or_else(|| array_type_index.to_string())
        ),
        value,
        expected_type_index_for_field(element, gc_type_info),
        gc_type_info,
    )
}

fn validate_ref_value_boundary(
    context: &str,
    value: StackValue,
    expected_type_index: Option<u32>,
    gc_type_info: &GcTypeInfo,
) -> Result<()> {
    if value.provenance == RefProvenance::NonRef {
        if expected_type_index.is_some() {
            bail!("ordinary WasmGC reference enters {context} without make_txn_ref");
        }
        return Ok(());
    }
    if value.provenance == RefProvenance::NullRef {
        return Ok(());
    }
    let Some(type_index) = value.type_index.or(expected_type_index) else {
        bail!("ordinary WasmGC reference enters {context} without make_txn_ref");
    };
    if value.provenance == RefProvenance::UnknownRef {
        bail!("ordinary WasmGC reference enters {context} without make_txn_ref");
    }
    ensure!(
        is_copyable_type(gc_type_info, type_index),
        "{context} references non-copyable GC type index {type_index}"
    );
    if !value.provenance.can_enter_persistent_value() {
        bail!("ordinary WasmGC reference enters {context} without make_txn_ref");
    }
    Ok(())
}

fn sidecar_struct_fields_for_boundary(
    struct_type_index: u32,
    gc_type_info: &GcTypeInfo,
) -> Vec<(u32, &KotlinField)> {
    gc_type_info
        .explicit_persistent_struct_fields
        .iter()
        .chain(gc_type_info.copyable_struct_fields.iter())
        .filter_map(|((type_index, field_index), field)| {
            (*type_index == struct_type_index).then_some((*field_index, field))
        })
        .collect()
}

fn sidecar_struct_field_for_boundary<'a>(
    struct_type_index: u32,
    field_index: u32,
    gc_type_info: &'a GcTypeInfo,
) -> Option<&'a KotlinField> {
    gc_type_info
        .explicit_persistent_struct_fields
        .get(&(struct_type_index, field_index))
        .or_else(|| {
            gc_type_info
                .copyable_struct_fields
                .get(&(struct_type_index, field_index))
        })
}

fn sidecar_array_element_for_boundary(
    array_type_index: u32,
    gc_type_info: &GcTypeInfo,
) -> Option<&KotlinField> {
    gc_type_info
        .explicit_persistent_array_elements
        .get(&array_type_index)
        .or_else(|| gc_type_info.copyable_array_elements.get(&array_type_index))
}

fn expected_type_index_for_field(field: &KotlinField, gc_type_info: &GcTypeInfo) -> Option<u32> {
    field
        .r#type
        .as_deref()
        .and_then(|name| gc_type_info.sidecar_type_indices.get(name).copied())
}

fn field_boundary_context(
    struct_type_index: u32,
    field_index: u32,
    field: &KotlinField,
    gc_type_info: &GcTypeInfo,
) -> String {
    let type_name = type_name_for_index(struct_type_index, gc_type_info)
        .unwrap_or_else(|| struct_type_index.to_string());
    let field_name = if field.name.is_empty() {
        field_index.to_string()
    } else {
        field.name.clone()
    };
    format!("persistent field {type_name}.{field_name}")
}

fn stack_value_for_read_field(
    ty: Option<ParserValType>,
    receiver: StackValue,
    gc_type_info: &GcTypeInfo,
) -> StackValue {
    let Some(ty) = ty else {
        return StackValue::non_ref();
    };
    let mut value = stack_value_for_type(ty, gc_type_info);
    if value.provenance != RefProvenance::NonRef {
        value.provenance = if receiver.provenance == RefProvenance::PersistentRef {
            RefProvenance::PersistentRef
        } else {
            RefProvenance::OrdinaryRef
        };
    }
    value
}

fn stack_value_for_constructor(type_index: u32, gc_type_info: &GcTypeInfo) -> StackValue {
    let provenance = if is_declared_copyable_constructor_type(gc_type_info, type_index) {
        RefProvenance::TxnLocalCopyable
    } else {
        RefProvenance::OrdinaryRef
    };
    StackValue::ref_value(Some(type_index), provenance)
}

fn stack_value_for_type(ty: ParserValType, gc_type_info: &GcTypeInfo) -> StackValue {
    let Some(type_index) = module_ref_type_index(ty) else {
        return StackValue::non_ref();
    };
    let provenance = if is_explicit_persistent_type(gc_type_info, type_index) {
        RefProvenance::PersistentRef
    } else {
        RefProvenance::OrdinaryRef
    };
    StackValue::ref_value(Some(type_index), provenance)
}

fn struct_requires_source_boundary(type_index: u32, gc_type_info: &GcTypeInfo) -> bool {
    gc_type_info
        .explicit_persistent
        .structs
        .contains(&type_index)
        || gc_type_info.copyable_type_indices.values().any(|index| {
            *index == type_index && gc_type_info.copyable.structs.contains(&type_index)
        })
}

fn array_requires_source_boundary(type_index: u32, gc_type_info: &GcTypeInfo) -> bool {
    gc_type_info
        .explicit_persistent
        .arrays
        .contains(&type_index)
        || gc_type_info
            .copyable_type_indices
            .values()
            .any(|index| *index == type_index && gc_type_info.copyable.arrays.contains(&type_index))
}

fn is_copyable_type(gc_type_info: &GcTypeInfo, type_index: u32) -> bool {
    gc_type_info.copyable.structs.contains(&type_index)
        || gc_type_info.copyable.arrays.contains(&type_index)
}

fn is_explicit_persistent_type(gc_type_info: &GcTypeInfo, type_index: u32) -> bool {
    gc_type_info
        .explicit_persistent
        .structs
        .contains(&type_index)
        || gc_type_info
            .explicit_persistent
            .arrays
            .contains(&type_index)
}

fn is_declared_copyable_constructor_type(gc_type_info: &GcTypeInfo, type_index: u32) -> bool {
    is_explicit_persistent_type(gc_type_info, type_index)
        || gc_type_info
            .copyable_type_indices
            .values()
            .any(|copyable_index| *copyable_index == type_index)
}

fn type_name_for_index(type_index: u32, gc_type_info: &GcTypeInfo) -> Option<String> {
    gc_type_info
        .sidecar_type_indices
        .iter()
        .find_map(|(name, index)| (*index == type_index).then_some(name.clone()))
}

fn module_ref_type_index(ty: ParserValType) -> Option<u32> {
    match ty {
        ParserValType::Ref(ref_type) => ref_type.type_index()?.unpack().as_module_index(),
        _ => None,
    }
}

fn local_type_index(local_types: &[ParserValType], local_index: u32) -> Option<u32> {
    local_types
        .get(local_index as usize)
        .copied()
        .and_then(module_ref_type_index)
}

fn heap_type_index(heap_type: wasmparser::HeapType) -> Option<u32> {
    match heap_type {
        wasmparser::HeapType::Concrete(index) | wasmparser::HeapType::Exact(index) => {
            index.as_module_index()
        }
        _ => None,
    }
}

fn encode_transaction_objects(transaction_objects: &TransactionObjects) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(TRANSACTION_OBJECTS_VERSION);
    encode_index_set(&transaction_objects.memories, &mut bytes);
    encode_index_set(&transaction_objects.globals, &mut bytes);
    encode_index_set(&transaction_objects.functions, &mut bytes);
    encode_index_set(&transaction_objects.tables, &mut bytes);
    bytes
}

fn encode_index_set(indices: &BTreeSet<u32>, bytes: &mut Vec<u8>) {
    (indices.len() as u32).encode(bytes);
    for index in indices {
        index.encode(bytes);
    }
}

fn transaction_objects(input: &[u8]) -> Result<TransactionObjects> {
    let mut objects = TransactionObjects::default();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite transaction metadata payload")? {
            Payload::CustomSection(section)
                if section.name() == TRANSACTION_OBJECTS_CUSTOM_SECTION =>
            {
                let mut reader = BinaryReader::new(section.data(), 0);
                let version = reader
                    .read_u8()
                    .context("failed to parse transaction object metadata version")?;
                if version != TRANSACTION_OBJECTS_VERSION {
                    bail!(
                        "unsupported transaction object metadata version: expected {}, found {}",
                        TRANSACTION_OBJECTS_VERSION,
                        version
                    );
                }

                extend_index_set(&mut objects.memories, &mut reader, "memories")?;
                extend_index_set(&mut objects.globals, &mut reader, "globals")?;
                extend_index_set(&mut objects.functions, &mut reader, "functions")?;
                if !reader.eof() {
                    extend_index_set(&mut objects.tables, &mut reader, "tables")?;
                }
                if !reader.eof() {
                    bail!("transaction object metadata has trailing bytes");
                }
            }
            _ => {}
        }
    }

    Ok(objects)
}

fn function_index_map(
    input: &[u8],
    stripped_function_imports: &BTreeSet<u32>,
) -> Result<Vec<Option<u32>>> {
    let mut function_index_map = Vec::new();
    let mut stripped_seen = BTreeSet::new();
    let mut next_new_function_index = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite function-index payload")? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import =
                        import.context("failed to parse Kotlin rewrite function import")?;
                    if !matches!(import.ty, TypeRef::Func(_) | TypeRef::FuncExact(_)) {
                        continue;
                    }

                    let old_function_index = u32::try_from(function_index_map.len())
                        .context("Kotlin rewrite function index overflow")?;
                    if stripped_function_imports.contains(&old_function_index) {
                        stripped_seen.insert(old_function_index);
                        function_index_map.push(None);
                    } else {
                        function_index_map.push(Some(next_new_function_index));
                        next_new_function_index = next_new_function_index
                            .checked_add(1)
                            .context("Kotlin rewrite function index overflow")?;
                    }
                }
            }
            Payload::FunctionSection(section) => {
                for type_index in section {
                    type_index.context("failed to parse Kotlin rewrite function type index")?;
                    function_index_map.push(Some(next_new_function_index));
                    next_new_function_index = next_new_function_index
                        .checked_add(1)
                        .context("Kotlin rewrite function index overflow")?;
                }
            }
            _ => {}
        }
    }

    ensure!(
        stripped_seen == *stripped_function_imports,
        "Kotlin marker import index set contained non-import function indices"
    );
    Ok(function_index_map)
}

fn remap_transaction_objects(
    transaction_objects: &mut TransactionObjects,
    index_remapper: &KotlinIndexRemapper,
) -> Result<()> {
    transaction_objects.functions = transaction_objects
        .functions
        .iter()
        .map(|index| index_remapper.remap_function_index(*index))
        .collect::<Result<BTreeSet<_>>>()?;
    Ok(())
}

fn rewrite_kotlin_import_section(
    index_remapper: &mut KotlinIndexRemapper,
    imports: &mut ImportSection,
    section: wasmparser::ImportSectionReader<'_>,
) -> Result<()> {
    let mut next_function_index = 0u32;

    for import in section.into_imports() {
        let import = import.context("failed to parse Kotlin rewrite import")?;
        if matches!(import.ty, TypeRef::Func(_) | TypeRef::FuncExact(_)) {
            let old_function_index = next_function_index;
            next_function_index = next_function_index
                .checked_add(1)
                .context("Kotlin rewrite function index overflow")?;
            if index_remapper.function_index_removed(old_function_index) {
                continue;
            }
        }

        imports.import(
            import.module,
            import.name,
            index_remapper.entity_type(import.ty)?,
        );
    }

    Ok(())
}

fn explicit_root_marker_imports(
    input: &[u8],
    roots: &[RootGlobal],
    unit_getter_result: Option<ParserValType>,
) -> Result<(BTreeMap<u32, RootGlobal>, BTreeMap<u32, RootGlobal>)> {
    let signatures = function_type_signatures(input)?;
    let roots_by_name = roots
        .iter()
        .map(|root| (root.name.as_str(), root))
        .collect::<BTreeMap<_, _>>();
    let mut get_imports = BTreeMap::new();
    let mut set_imports = BTreeMap::new();
    let mut next_function_index = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin root marker import payload")? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin root marker import")?;
                    let type_index = match import.ty {
                        TypeRef::Func(type_index) | TypeRef::FuncExact(type_index) => type_index,
                        _ => {
                            continue;
                        }
                    };
                    let function_index = next_function_index;
                    next_function_index = next_function_index
                        .checked_add(1)
                        .context("Kotlin root marker function index overflow")?;

                    let marker_kind = match import.module {
                        KOTLIN_ROOT_GET_IMPORT_MODULE => Some("get"),
                        KOTLIN_ROOT_SET_IMPORT_MODULE => Some("set"),
                        _ => None,
                    };
                    let Some(marker_kind) = marker_kind else {
                        continue;
                    };
                    let root = roots_by_name.get(import.name).with_context(|| {
                        format!(
                            "Kotlin root marker import {}.{} names unknown root",
                            import.module, import.name
                        )
                    })?;
                    let signature = signatures.get(&type_index).with_context(|| {
                        format!(
                            "Kotlin root marker import {}.{} uses unknown function type {type_index}",
                            import.module, import.name
                        )
                    })?;
                    match marker_kind {
                        "get" => {
                            ensure!(
                                signature.params.is_empty(),
                                "Kotlin root get marker {} must not have parameters",
                                import.name
                            );
                            ensure!(
                                signature.results.len() == 1,
                                "Kotlin root get marker {} must return one value",
                                import.name
                            );
                            ensure_marker_ref_type(
                                signature.results[0],
                                root,
                                "Kotlin root get marker",
                            )?;
                            if get_imports
                                .insert(function_index, (*root).clone())
                                .is_some()
                            {
                                bail!(
                                    "duplicate Kotlin root get marker function index {function_index}"
                                );
                            }
                        }
                        "set" => {
                            ensure!(
                                signature.params.len() == 1,
                                "Kotlin root set marker {} must have one parameter",
                                import.name
                            );
                            ensure!(
                                signature.results.len() == 1,
                                "Kotlin root set marker {} must return Kotlin Unit",
                                import.name
                            );
                            let unit_getter_result = unit_getter_result.with_context(|| {
                                format!(
                                    "Kotlin root set marker {} requires kotlin.Unit_getInstance",
                                    import.name
                                )
                            })?;
                            ensure!(
                                signature.results[0] == unit_getter_result,
                                "Kotlin root set marker {} must return kotlin.Unit_getInstance type",
                                import.name
                            );
                            ensure_marker_ref_type(
                                signature.params[0],
                                root,
                                "Kotlin root set marker",
                            )?;
                            if set_imports
                                .insert(function_index, (*root).clone())
                                .is_some()
                            {
                                bail!(
                                    "duplicate Kotlin root set marker function index {function_index}"
                                );
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
            _ => {}
        }
    }

    Ok((get_imports, set_imports))
}

fn ensure_marker_ref_type(value_type: ParserValType, root: &RootGlobal, label: &str) -> Result<()> {
    let Some(type_index) = nullable_concrete_module_ref_type_index(value_type) else {
        bail!("{label} {} must use nullable concrete root ref", root.name);
    };
    ensure!(
        type_index == root.type_index,
        "{label} {} has type index {type_index}, expected {}",
        root.name,
        root.type_index
    );
    Ok(())
}

fn nullable_concrete_module_ref_type_index(ty: ParserValType) -> Option<u32> {
    let ParserValType::Ref(ref_type) = ty else {
        return None;
    };
    if !ref_type.is_nullable() || ref_type.is_exact_type_ref() {
        return None;
    }
    ref_type.type_index()?.unpack().as_module_index()
}

fn extend_index_set(
    indices: &mut BTreeSet<u32>,
    reader: &mut BinaryReader<'_>,
    label: &str,
) -> Result<()> {
    let len = reader
        .read_var_u32()
        .with_context(|| format!("failed to parse transaction object {label} length"))?;
    for _ in 0..len {
        indices.insert(
            reader
                .read_var_u32()
                .with_context(|| format!("failed to parse transaction object {label} index"))?,
        );
    }
    Ok(())
}

fn exported_function_names(input: &[u8]) -> Result<BTreeMap<String, u32>> {
    let mut exports = BTreeMap::new();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite export payload")? {
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.context("failed to parse Kotlin rewrite export")?;
                    if matches!(export.kind, ExternalKind::Func | ExternalKind::FuncExact) {
                        exports.insert(export.name.to_string(), export.index);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(exports)
}

fn transaction_function_indices(input: &[u8], sidecar: &KotlinSidecar) -> Result<BTreeSet<u32>> {
    let mut indices = BTreeSet::new();
    let exported_functions = exported_function_names(input)?;
    let function_name_entries = name_section_function_names(input)?;

    for name in &sidecar.transaction_functions {
        if let Some(index) = exported_functions.get(name) {
            indices.insert(*index);
            continue;
        }

        let matches = function_name_entries
            .iter()
            .filter_map(|(candidate, index)| (candidate == name).then_some(*index))
            .collect::<BTreeSet<_>>();
        match matches.len() {
            0 => bail!("transaction function {name} was not found"),
            1 => {
                indices.insert(*matches.iter().next().unwrap());
            }
            _ => bail!("ambiguous Kotlin rewrite function name {name}: {matches:?}"),
        };
    }

    Ok(indices)
}

fn transaction_call_closure_function_indices(
    input: &[u8],
    transaction_function_indices: &BTreeSet<u32>,
    imported_function_count: u32,
) -> Result<BTreeSet<u32>> {
    let call_edges = direct_local_call_edges(input, imported_function_count)?;
    function_call_closure(transaction_function_indices, &call_edges)
}

fn ordinary_shared_function_indices(input: &[u8]) -> Result<BTreeSet<u32>> {
    let mut indices = BTreeSet::new();
    let mut function_entries = exported_function_names(input)?
        .into_iter()
        .map(|(name, index)| (name, index))
        .collect::<Vec<_>>();
    function_entries.extend(name_section_function_names(input)?);

    for (name, index) in function_entries {
        if name.starts_with("kotlin.")
            || name.contains(".<init>")
            || name == "_initializeModule"
            || name == "_stringLiteralLatin1"
        {
            indices.insert(index);
        }
    }

    Ok(indices)
}

fn function_call_closure(
    seeds: &BTreeSet<u32>,
    call_edges: &BTreeMap<u32, BTreeSet<u32>>,
) -> Result<BTreeSet<u32>> {
    let mut closure = seeds.clone();
    let mut worklist = seeds.iter().copied().collect::<Vec<_>>();
    while let Some(function_index) = worklist.pop() {
        let Some(callees) = call_edges.get(&function_index) else {
            continue;
        };
        for callee in callees {
            if closure.insert(*callee) {
                worklist.push(*callee);
            }
        }
    }

    Ok(closure)
}

fn direct_local_call_edges(
    input: &[u8],
    imported_function_count: u32,
) -> Result<BTreeMap<u32, BTreeSet<u32>>> {
    let mut edges = BTreeMap::<u32, BTreeSet<u32>>::new();
    let mut next_defined_func = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite call graph payload")? {
            Payload::CodeSectionStart { range, .. } => {
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid Kotlin rewrite code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;

                for body in section {
                    let body = body?;
                    let caller = imported_function_count
                        .checked_add(next_defined_func)
                        .context("Kotlin rewrite function index overflow")?;
                    let mut reader = body.get_operators_reader()?;
                    while !reader.eof() {
                        if let Operator::Call { function_index } = reader.read()?
                            && function_index >= imported_function_count
                        {
                            edges.entry(caller).or_default().insert(function_index);
                        }
                    }
                    next_defined_func += 1;
                }
            }
            Payload::CodeSectionEntry(_) => {}
            _ => {}
        }
    }

    Ok(edges)
}

fn persistent_accessor_function_indices(
    input: &[u8],
    sidecar: &KotlinSidecar,
) -> Result<BTreeSet<u32>> {
    let mut accessor_names = BTreeSet::new();
    for persistent_type in &sidecar.persistent_types {
        if persistent_type.kind != KotlinPersistentKind::Struct {
            continue;
        }
        for field in &persistent_type.fields {
            accessor_names.insert(format!("{}.<get-{}>", persistent_type.name, field.name));
            accessor_names.insert(format!("{}.<set-{}>", persistent_type.name, field.name));
        }
    }

    let mut function_entries = exported_function_names(input)?
        .into_iter()
        .map(|(name, index)| (name, index))
        .collect::<Vec<_>>();
    function_entries.extend(name_section_function_names(input)?);

    let mut indices = BTreeSet::new();
    for accessor_name in accessor_names {
        let matches = function_entries
            .iter()
            .filter_map(|(name, index)| (name == &accessor_name).then_some(*index))
            .collect::<BTreeSet<_>>();
        match matches.len() {
            0 => {}
            1 => {
                indices.insert(*matches.iter().next().unwrap());
            }
            _ => bail!("ambiguous Kotlin rewrite persistent accessor {accessor_name}: {matches:?}"),
        }
    }

    Ok(indices)
}

fn imported_function_count(input: &[u8]) -> Result<u32> {
    let len = imported_function_type_indices(input)?.len();
    u32::try_from(len).context("Kotlin rewrite imported function count overflow")
}

fn imported_function_type_indices(input: &[u8]) -> Result<Vec<u32>> {
    let mut type_indices = Vec::new();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite import payload")? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin rewrite import")?;
                    match import.ty {
                        TypeRef::Func(type_index) | TypeRef::FuncExact(type_index) => {
                            type_indices.push(type_index);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(type_indices)
}

fn function_signature(input: &[u8], function_index: u32) -> Result<Option<FunctionSignature>> {
    let signatures = function_type_signatures(input)?;
    let imported_function_types = imported_function_type_indices(input)?;
    let imported_function_count = u32::try_from(imported_function_types.len())
        .context("Kotlin rewrite imported function count overflow")?;
    let type_index = if let Some(type_index) = imported_function_types.get(function_index as usize)
    {
        *type_index
    } else {
        let defined_index = function_index
            .checked_sub(imported_function_count)
            .context("Kotlin rewrite function index underflow")?;
        let defined_function_types = defined_function_type_indices(input)?;
        let Some(type_index) = defined_function_types.get(defined_index as usize) else {
            return Ok(None);
        };
        *type_index
    };

    Ok(signatures.get(&type_index).cloned())
}

fn imported_global_count(input: &[u8]) -> Result<u32> {
    let mut count = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite import payload")? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin rewrite import")?;
                    if matches!(import.ty, TypeRef::Global(_)) {
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(count)
}

fn defined_global_count(input: &[u8]) -> Result<u32> {
    let mut count = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite global payload")? {
            Payload::GlobalSection(section) => {
                count = count
                    .checked_add(section.count())
                    .context("Kotlin rewrite global count overflow")?;
            }
            _ => {}
        }
    }

    Ok(count)
}

fn function_index_by_exact_name(input: &[u8], expected: &str) -> Result<Option<u32>> {
    let matches = function_indices_by_name(input, |name| name == expected)?;
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(*matches.iter().next().unwrap())),
        _ => bail!("ambiguous Kotlin rewrite function name {expected}: {matches:?}"),
    }
}

fn txref_get_function_indices(input: &[u8]) -> Result<BTreeSet<u32>> {
    function_indices_by_name(input, |name| {
        name == "twasm.TxRef.get"
            || name.ends_with(".TxRef.get")
            || name.ends_with("TxRef.<get-value>")
            || (name.contains("TxRef") && name.ends_with(".get"))
    })
}

fn function_indices_by_name(
    input: &[u8],
    mut predicate: impl FnMut(&str) -> bool,
) -> Result<BTreeSet<u32>> {
    let mut function_entries = exported_function_names(input)?
        .into_iter()
        .map(|(name, index)| (name, index))
        .collect::<Vec<_>>();
    function_entries.extend(name_section_function_names(input)?);

    Ok(function_entries
        .iter()
        .filter_map(|(name, index)| predicate(name).then_some(*index))
        .collect())
}

fn root_marker_function_indices(
    input: &[u8],
    root_lowering: &RootLowering,
    imported_function_count: u32,
    func_type_params: &BTreeMap<u32, Vec<ParserValType>>,
    defined_function_types: &[u32],
    marker_literals: &KotlinMarkerLiterals,
) -> Result<BTreeMap<u32, RootMarkerUses>> {
    let mut indices = BTreeMap::new();
    if root_lowering.globals.is_empty() {
        return Ok(indices);
    }
    let mut next_defined_func = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite root marker payload")? {
            Payload::CodeSectionStart { range, .. } => {
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid Kotlin rewrite code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;

                for body in section {
                    let body = body?;
                    let function_index = imported_function_count + next_defined_func;
                    let type_index = *defined_function_types
                        .get(next_defined_func as usize)
                        .context("missing Kotlin rewrite function type index")?;
                    let params = func_type_params
                        .get(&type_index)
                        .cloned()
                        .context("missing Kotlin rewrite function type parameters")?;
                    let uses =
                        function_root_marker_uses(&body, &params, root_lowering, marker_literals)?;
                    if uses.any() {
                        indices.insert(function_index, uses);
                    }
                    next_defined_func += 1;
                }
            }
            Payload::CodeSectionEntry(_) => {}
            _ => {}
        }
    }

    Ok(indices)
}

#[derive(Clone, Copy, Default)]
struct RootMarkerUses {
    get: bool,
    set: bool,
}

impl RootMarkerUses {
    fn any(self) -> bool {
        self.get || self.set
    }
}

fn function_root_marker_uses(
    body: &wasmparser::FunctionBody<'_>,
    params: &[ParserValType],
    root_lowering: &RootLowering,
    marker_literals: &KotlinMarkerLiterals,
) -> Result<RootMarkerUses> {
    let local_types = local_types(body, params)?;
    let mut reader = body.get_operators_reader()?;
    let mut operators = Vec::new();
    while !reader.eof() {
        operators.push(reader.read()?);
    }

    let mut uses = RootMarkerUses::default();
    for index in 0..operators.len() {
        if let Some(Operator::Call { function_index }) = operators.get(index) {
            if root_lowering.get_imports.contains_key(function_index) {
                uses.get = true;
            }
            if root_lowering.set_imports.contains_key(function_index) {
                uses.set = true;
            }
        }
        if let Some(marker) =
            match_inline_set_root_marker(&operators, index, &local_types, marker_literals)?
        {
            if root_for_marker_type(root_lowering, marker.type_index)?.is_some() {
                uses.set = true;
            }
        }
        if let Some(marker) = match_inline_get_root_marker(&operators, index, marker_literals)?
            && root_for_marker_type(root_lowering, marker.type_index)?.is_some()
        {
            uses.get = true;
        }
    }
    Ok(uses)
}

fn function_type_params(input: &[u8]) -> Result<BTreeMap<u32, Vec<ParserValType>>> {
    Ok(function_type_signatures(input)?
        .into_iter()
        .map(|(type_index, signature)| (type_index, signature.params))
        .collect())
}

fn function_type_signatures(input: &[u8]) -> Result<BTreeMap<u32, FunctionSignature>> {
    let mut signatures = BTreeMap::new();
    let mut next_type_index = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite type payload")? {
            Payload::TypeSection(section) => {
                for group in section {
                    let group = group.context("failed to parse Kotlin rewrite type group")?;
                    for ty in group.into_types() {
                        if let CompositeInnerType::Func(func) = ty.composite_type.inner {
                            signatures.insert(
                                next_type_index,
                                FunctionSignature {
                                    params: func.params().to_vec(),
                                    results: func.results().to_vec(),
                                },
                            );
                        }
                        next_type_index += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(signatures)
}

fn function_signatures_by_index(input: &[u8]) -> Result<BTreeMap<u32, FunctionSignature>> {
    let type_signatures = function_type_signatures(input)?;
    let mut function_signatures = BTreeMap::new();
    for (function_index, type_index) in imported_function_type_indices(input)?
        .into_iter()
        .chain(defined_function_type_indices(input)?.into_iter())
        .enumerate()
    {
        if let Some(signature) = type_signatures.get(&type_index).cloned() {
            function_signatures.insert(
                u32::try_from(function_index).context("Kotlin rewrite function index overflow")?,
                signature,
            );
        }
    }
    Ok(function_signatures)
}

fn defined_function_type_indices(input: &[u8]) -> Result<Vec<u32>> {
    let mut type_indices = Vec::new();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite function payload")? {
            Payload::FunctionSection(section) => {
                for type_index in section {
                    type_indices.push(
                        type_index.context("failed to parse Kotlin rewrite function type index")?,
                    );
                }
            }
            _ => {}
        }
    }

    Ok(type_indices)
}

fn gc_type_info(input: &[u8], sidecar: &KotlinSidecar) -> Result<GcTypeInfo> {
    let mut gc_type_indices = Vec::new();
    let mut info = GcTypeInfo::default();
    let mut next_type_index = 0u32;
    let type_names = name_section_type_names(input)?;
    info.struct_field_names = name_section_field_names(input)?;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite type payload")? {
            Payload::TypeSection(section) => {
                for group in section {
                    let group = group.context("failed to parse Kotlin rewrite type group")?;
                    for ty in group.into_types() {
                        match ty.composite_type.inner {
                            CompositeInnerType::Struct(struct_ty) => {
                                info.struct_field_counts
                                    .insert(next_type_index, struct_ty.fields.len());
                                for (field_index, field) in struct_ty.fields.iter().enumerate() {
                                    info.struct_field_storage.insert(
                                        (next_type_index, field_index as u32),
                                        field.element_type,
                                    );
                                    info.struct_fields.insert(
                                        (next_type_index, field_index as u32),
                                        field.element_type.unpack(),
                                    );
                                }
                                gc_type_indices
                                    .push((next_type_index, KotlinPersistentKind::Struct));
                            }
                            CompositeInnerType::Array(array_ty) => {
                                info.array_element_storage
                                    .insert(next_type_index, array_ty.0.element_type);
                                info.array_elements
                                    .insert(next_type_index, array_ty.0.element_type.unpack());
                                gc_type_indices
                                    .push((next_type_index, KotlinPersistentKind::Array));
                            }
                            _ => {}
                        }
                        next_type_index += 1;
                    }
                }
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin rewrite import")?;
                    if let TypeRef::Global(global) = import.ty {
                        let global_index = u32::try_from(info.globals.len())
                            .context("Kotlin rewrite global index overflow")?;
                        info.globals.insert(global_index, global.content_type);
                    }
                }
            }
            Payload::GlobalSection(section) => {
                for global in section {
                    let global = global.context("failed to parse Kotlin rewrite global")?;
                    let global_index = u32::try_from(info.globals.len())
                        .context("Kotlin rewrite global index overflow")?;
                    info.globals.insert(global_index, global.ty.content_type);
                }
            }
            _ => {}
        }
    }

    if sidecar.gc_wasm.capture == KotlinGcWasmCapture::AllModuleGcTypes {
        for (name, indices) in &type_names {
            let denied = kotlin_type_denied(name, &sidecar.gc_wasm.deny_types);
            let gc_indices = indices
                .iter()
                .copied()
                .filter(|type_index| gc_type_kind(&info, *type_index).is_some())
                .collect::<Vec<_>>();
            if gc_indices.len() == 1 {
                info.module_type_indices.insert(name.clone(), gc_indices[0]);
            }
            for type_index in gc_indices {
                if denied {
                    info.denied_type_indices.insert(type_index);
                    continue;
                }
                match gc_type_kind(&info, type_index).context("missing Kotlin rewrite GC kind")? {
                    KotlinPersistentKind::Struct => {
                        info.persistent.structs.insert(type_index);
                    }
                    KotlinPersistentKind::Array => {
                        info.persistent.arrays.insert(type_index);
                    }
                }
            }
        }

        for (type_index, kind) in &gc_type_indices {
            if info.denied_type_indices.contains(type_index) {
                continue;
            }
            match kind {
                KotlinPersistentKind::Struct => {
                    info.persistent.structs.insert(*type_index);
                }
                KotlinPersistentKind::Array => {
                    info.persistent.arrays.insert(*type_index);
                }
            }
        }
    }

    let mut mapped_type_indices = BTreeSet::new();
    let mut fallback_ordinal = 0usize;
    for persistent_type in &sidecar.persistent_types {
        let (type_index, actual_kind) = if let Some(type_index) =
            info.module_type_indices.get(&persistent_type.name).copied()
        {
            ensure_type_not_denied(
                &info,
                &sidecar.gc_wasm.deny_types,
                &persistent_type.name,
                type_index,
                &format!("persistent type {}", persistent_type.name),
            )?;
            let actual_kind = gc_type_kind(&info, type_index)
                .context("named persistent type did not map to a GC type")?;
            let gc_position = gc_type_indices
                .iter()
                .position(|(candidate, _)| *candidate == type_index)
                .context("named persistent type did not map to a GC type")?;
            fallback_ordinal = fallback_ordinal.max(gc_position + 1);
            (type_index, actual_kind)
        } else if let Some(type_indices) = type_names.get(&persistent_type.name) {
            let gc_indices = type_indices
                .iter()
                .copied()
                .filter(|type_index| gc_type_kind(&info, *type_index).is_some())
                .collect::<BTreeSet<_>>();
            if gc_indices.len() > 1 {
                bail!(
                    "ambiguous Kotlin rewrite type name {}: {:?}",
                    persistent_type.name,
                    gc_indices
                );
            }
            let Some(type_index) = gc_indices.iter().next().copied() else {
                bail!(
                    "persistent type {} maps to non-GC type",
                    persistent_type.name
                );
            };
            ensure_type_not_denied(
                &info,
                &sidecar.gc_wasm.deny_types,
                &persistent_type.name,
                type_index,
                &format!("persistent type {}", persistent_type.name),
            )?;
            let actual_kind = gc_type_kind(&info, type_index)
                .context("named persistent type did not map to a GC type")?;
            let gc_position = gc_type_indices
                .iter()
                .position(|(candidate, _)| *candidate == type_index)
                .context("named persistent type did not map to a GC type")?;
            fallback_ordinal = fallback_ordinal.max(gc_position + 1);
            (type_index, actual_kind)
        } else {
            while gc_type_indices
                .get(fallback_ordinal)
                .is_some_and(|(candidate, _)| {
                    mapped_type_indices.contains(candidate)
                        || info.denied_type_indices.contains(candidate)
                })
            {
                fallback_ordinal += 1;
            }
            let Some((type_index, actual_kind)) = gc_type_indices.get(fallback_ordinal).copied()
            else {
                bail!(
                    "persistent type {} could not be mapped",
                    persistent_type.name
                );
            };
            fallback_ordinal += 1;
            (type_index, actual_kind)
        };

        if !mapped_type_indices.insert(type_index) {
            bail!(
                "persistent type {} maps to duplicate GC type index {}",
                persistent_type.name,
                type_index
            );
        }

        if actual_kind != persistent_type.kind {
            bail!(
                "persistent type {} expected {:?} at GC type index {}, found {:?}",
                persistent_type.name,
                persistent_type.kind,
                type_index,
                actual_kind
            );
        }

        info.sidecar_type_indices
            .insert(persistent_type.name.clone(), type_index);

        match persistent_type.kind {
            KotlinPersistentKind::Struct => {
                info.persistent.structs.insert(type_index);
                info.explicit_persistent.structs.insert(type_index);
                info.copyable.structs.insert(type_index);
            }
            KotlinPersistentKind::Array => {
                info.persistent.arrays.insert(type_index);
                info.explicit_persistent.arrays.insert(type_index);
                info.copyable.arrays.insert(type_index);
            }
        }
    }

    for copyable_type in &sidecar.copyable_types {
        ensure_runtime_type_mapping(
            &mut info,
            &type_names,
            &sidecar.gc_wasm.deny_types,
            &copyable_type.name,
            &format!("copyable type {}", copyable_type.name),
        )?;
        let type_index = info.sidecar_type_indices[&copyable_type.name];
        let actual_kind = gc_type_kind(&info, type_index)
            .context("named copyable type did not map to a GC type")?;
        if actual_kind != copyable_type.kind {
            bail!(
                "copyable type {} expected {:?} at GC type index {}, found {:?}",
                copyable_type.name,
                copyable_type.kind,
                type_index,
                actual_kind
            );
        }
        info.copyable_type_indices
            .insert(copyable_type.name.clone(), type_index);
        match copyable_type.kind {
            KotlinPersistentKind::Struct => {
                info.copyable.structs.insert(type_index);
            }
            KotlinPersistentKind::Array => {
                info.copyable.arrays.insert(type_index);
            }
        }
    }

    for root in &sidecar.roots {
        ensure_runtime_type_mapping(
            &mut info,
            &type_names,
            &sidecar.gc_wasm.deny_types,
            &root.r#type,
            &format!("Kotlin root {}", root.name),
        )?;
    }
    for persistent_type in &sidecar.persistent_types {
        for field in persistent_type
            .fields
            .iter()
            .chain(persistent_type.element.iter())
        {
            let Some(target_name) = field.r#type.as_deref() else {
                continue;
            };
            ensure_runtime_type_mapping(
                &mut info,
                &type_names,
                &sidecar.gc_wasm.deny_types,
                target_name,
                &format!(
                    "persistent type {} field {}",
                    persistent_type.name, field.name
                ),
            )?;
        }
    }
    for copyable_type in &sidecar.copyable_types {
        for field in copyable_type
            .fields
            .iter()
            .chain(copyable_type.element.iter())
        {
            let Some(target_name) = field.r#type.as_deref() else {
                continue;
            };
            ensure_runtime_type_mapping(
                &mut info,
                &type_names,
                &sidecar.gc_wasm.deny_types,
                target_name,
                &format!(
                    "copyable type {} field {} references denied type {}",
                    copyable_type.name, field.name, target_name
                ),
            )?;
        }
    }

    mark_builtin_copyable_types(&mut info);

    for persistent_type in &sidecar.persistent_types {
        let type_index = info.sidecar_type_indices[&persistent_type.name];
        validate_persistent_type_shape(type_index, persistent_type, &info)?;
        record_sidecar_boundary_fields(type_index, persistent_type, true, &mut info)?;
    }
    for copyable_type in &sidecar.copyable_types {
        let type_index = info.copyable_type_indices[&copyable_type.name];
        record_copyable_boundary_fields(type_index, copyable_type, &mut info)?;
    }

    Ok(info)
}

fn gc_type_kind(info: &GcTypeInfo, type_index: u32) -> Option<KotlinPersistentKind> {
    if info.struct_field_counts.contains_key(&type_index) {
        Some(KotlinPersistentKind::Struct)
    } else if info.array_element_storage.contains_key(&type_index) {
        Some(KotlinPersistentKind::Array)
    } else {
        None
    }
}

fn ensure_runtime_type_mapping(
    info: &mut GcTypeInfo,
    type_names: &BTreeMap<String, BTreeSet<u32>>,
    deny_types: &[String],
    type_name: &str,
    context: &str,
) -> Result<()> {
    if let Some(type_index) = info.sidecar_type_indices.get(type_name).copied() {
        ensure_type_not_denied(info, deny_types, type_name, type_index, context)?;
        return Ok(());
    }

    let gc_indices = type_names
        .get(type_name)
        .into_iter()
        .flatten()
        .copied()
        .filter(|type_index| gc_type_kind(info, *type_index).is_some())
        .collect::<BTreeSet<_>>();

    if gc_indices.is_empty() {
        bail!("{context} has unmapped type {type_name}");
    }
    if kotlin_type_denied(type_name, deny_types)
        || gc_indices
            .iter()
            .any(|type_index| info.denied_type_indices.contains(type_index))
    {
        bail!("{context} uses denied Kotlin GC type {type_name}");
    }
    if gc_indices.len() != 1 {
        bail!(
            "{context} has ambiguous runtime GC type {type_name}: {:?}",
            gc_indices
        );
    }

    info.sidecar_type_indices
        .insert(type_name.to_string(), *gc_indices.iter().next().unwrap());
    Ok(())
}

fn ensure_type_not_denied(
    info: &GcTypeInfo,
    deny_types: &[String],
    type_name: &str,
    type_index: u32,
    context: &str,
) -> Result<()> {
    if kotlin_type_denied(type_name, deny_types) || info.denied_type_indices.contains(&type_index) {
        bail!("{context} uses denied Kotlin GC type {type_name}");
    }
    Ok(())
}

fn kotlin_type_denied(name: &str, deny_types: &[String]) -> bool {
    deny_types.iter().any(|pattern| {
        pattern.strip_suffix(".*").is_some_and(|prefix| {
            name == prefix
                || name
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        }) || pattern == name
    })
}

fn mark_builtin_copyable_types(info: &mut GcTypeInfo) {
    let builtin_indices = info
        .sidecar_type_indices
        .iter()
        .filter_map(|(name, type_index)| {
            is_builtin_copyable_type_name(name).then_some((*type_index, name.clone()))
        })
        .collect::<Vec<_>>();
    for (type_index, _name) in builtin_indices {
        match gc_type_kind(info, type_index) {
            Some(KotlinPersistentKind::Struct) => {
                info.copyable.structs.insert(type_index);
            }
            Some(KotlinPersistentKind::Array) => {
                info.copyable.arrays.insert(type_index);
            }
            None => {}
        }
    }
}

fn is_builtin_copyable_type_name(name: &str) -> bool {
    matches!(
        name,
        "kotlin.String"
            | "String"
            | "kotlin.Boolean"
            | "kotlin.Byte"
            | "kotlin.Short"
            | "kotlin.Int"
            | "kotlin.Long"
            | "kotlin.Float"
            | "kotlin.Double"
            | "kotlin.Char"
            | "kotlin.Unit"
    )
}

fn name_section_function_names(input: &[u8]) -> Result<Vec<(String, u32)>> {
    name_section_index_names(input, NameSectionIndexKind::Function)
}

fn name_section_type_names(input: &[u8]) -> Result<BTreeMap<String, BTreeSet<u32>>> {
    let mut names = BTreeMap::<String, BTreeSet<u32>>::new();
    for (name, index) in name_section_index_names(input, NameSectionIndexKind::Type)? {
        names.entry(name).or_default().insert(index);
    }
    Ok(names)
}

fn name_section_field_names(input: &[u8]) -> Result<BTreeMap<(u32, String), BTreeSet<u32>>> {
    let mut names = BTreeMap::<(u32, String), BTreeSet<u32>>::new();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite field-name payload")? {
            Payload::CustomSection(section) => {
                let KnownCustom::Name(name_section) = section.as_known() else {
                    continue;
                };
                for subsection in name_section {
                    let subsection = subsection
                        .context("failed to parse Kotlin rewrite field-name subsection")?;
                    let Name::Field(indirect_map) = subsection else {
                        continue;
                    };
                    for indirect in indirect_map {
                        let indirect = indirect
                            .context("failed to parse Kotlin rewrite field-name type entry")?;
                        for naming in indirect.names {
                            let naming = naming
                                .context("failed to parse Kotlin rewrite field-name entry")?;
                            names
                                .entry((indirect.index, naming.name.to_string()))
                                .or_default()
                                .insert(naming.index);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(names)
}

#[derive(Clone, Copy)]
enum NameSectionIndexKind {
    Function,
    Type,
}

fn name_section_index_names(
    input: &[u8],
    kind: NameSectionIndexKind,
) -> Result<Vec<(String, u32)>> {
    let mut names = Vec::new();

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite name payload")? {
            Payload::CustomSection(section) => {
                let KnownCustom::Name(name_section) = section.as_known() else {
                    continue;
                };
                for subsection in name_section {
                    let subsection =
                        subsection.context("failed to parse Kotlin rewrite name subsection")?;
                    match (kind, subsection) {
                        (NameSectionIndexKind::Function, Name::Function(map))
                        | (NameSectionIndexKind::Type, Name::Type(map)) => {
                            for naming in map {
                                let naming = naming.context(
                                    "failed to parse Kotlin rewrite name subsection entry",
                                )?;
                                names.push((naming.name.to_string(), naming.index));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(names)
}

fn validate_persistent_type_shape(
    type_index: u32,
    persistent_type: &KotlinPersistentType,
    info: &GcTypeInfo,
) -> Result<()> {
    match persistent_type.kind {
        KotlinPersistentKind::Struct => {
            let has_named_fields = info
                .struct_field_names
                .keys()
                .any(|(named_type_index, _)| *named_type_index == type_index);
            if !has_named_fields {
                let actual_count = *info
                    .struct_field_counts
                    .get(&type_index)
                    .context("missing Kotlin rewrite struct field count")?;
                if actual_count != persistent_type.fields.len() {
                    bail!(
                        "persistent type {} field count mismatch: sidecar has {}, WasmGC type has {}",
                        persistent_type.name,
                        persistent_type.fields.len(),
                        actual_count
                    );
                }
            } else {
                let actual_count = *info
                    .struct_field_counts
                    .get(&type_index)
                    .context("missing Kotlin rewrite struct field count")?;
                let named_count = info
                    .struct_field_names
                    .keys()
                    .filter(|(named_type_index, _)| *named_type_index == type_index)
                    .count();
                if named_count != actual_count {
                    bail!(
                        "persistent type {} has unnamed non-runtime WasmGC fields",
                        persistent_type.name
                    );
                }
                let sidecar_fields = persistent_type
                    .fields
                    .iter()
                    .map(|field| field.name.as_str())
                    .collect::<BTreeSet<_>>();
                for ((_, field_name), _) in info
                    .struct_field_names
                    .iter()
                    .filter(|((named_type_index, _), _)| *named_type_index == type_index)
                {
                    if KOTLIN_RUNTIME_STRUCT_FIELDS.contains(&field_name.as_str()) {
                        continue;
                    }
                    if !sidecar_fields.contains(field_name.as_str()) {
                        bail!(
                            "persistent type {} missing persistent field {} in sidecar",
                            persistent_type.name,
                            field_name
                        );
                    }
                }
            }

            for (ordinal, field) in persistent_type.fields.iter().enumerate() {
                let field_index = if has_named_fields {
                    let field_indices = info
                        .struct_field_names
                        .get(&(type_index, field.name.clone()))
                        .with_context(|| {
                            format!(
                                "persistent type {} field {} was not found in WasmGC field names",
                                persistent_type.name, field.name
                            )
                        })?;
                    if field_indices.len() != 1 {
                        bail!(
                            "ambiguous Kotlin rewrite field name {} on persistent type {}: {:?}",
                            field.name,
                            persistent_type.name,
                            field_indices
                        );
                    }
                    *field_indices.iter().next().unwrap()
                } else {
                    u32::try_from(ordinal).context("Kotlin sidecar field ordinal overflow")?
                };
                let actual = *info
                    .struct_field_storage
                    .get(&(type_index, field_index))
                    .context("missing Kotlin rewrite struct field type")?;
                validate_sidecar_field_type(
                    actual,
                    field,
                    &format!(
                        "persistent type {} field {}",
                        persistent_type.name, field.name
                    ),
                    info,
                )?;
            }
        }
        KotlinPersistentKind::Array => {
            let actual = *info
                .array_element_storage
                .get(&type_index)
                .context("missing Kotlin rewrite array element type")?;
            let element = persistent_type
                .element
                .as_ref()
                .context("array sidecar element was not validated")?;
            validate_sidecar_field_type(
                actual,
                element,
                &format!("persistent type {} element", persistent_type.name),
                info,
            )?;
        }
    }

    Ok(())
}

fn record_sidecar_boundary_fields(
    type_index: u32,
    persistent_type: &KotlinPersistentType,
    explicit_persistent: bool,
    info: &mut GcTypeInfo,
) -> Result<()> {
    match persistent_type.kind {
        KotlinPersistentKind::Struct => {
            let fields = sidecar_struct_field_indices(type_index, &persistent_type.fields, info)?;
            for (field_index, field) in fields {
                if explicit_persistent {
                    info.explicit_persistent_struct_fields
                        .insert((type_index, field_index), field.clone());
                } else {
                    info.copyable_struct_fields
                        .insert((type_index, field_index), field.clone());
                }
            }
        }
        KotlinPersistentKind::Array => {
            let Some(element) = persistent_type.element.clone() else {
                return Ok(());
            };
            if explicit_persistent {
                info.explicit_persistent_array_elements
                    .insert(type_index, element);
            } else {
                info.copyable_array_elements.insert(type_index, element);
            }
        }
    }
    Ok(())
}

fn record_copyable_boundary_fields(
    type_index: u32,
    copyable_type: &KotlinCopyableType,
    info: &mut GcTypeInfo,
) -> Result<()> {
    let persistent_like = KotlinPersistentType {
        name: copyable_type.name.clone(),
        kind: copyable_type.kind,
        fields: copyable_type.fields.clone(),
        element: copyable_type.element.clone(),
    };
    record_sidecar_boundary_fields(type_index, &persistent_like, false, info)
}

fn sidecar_struct_field_indices<'a>(
    type_index: u32,
    fields: &'a [KotlinField],
    info: &GcTypeInfo,
) -> Result<Vec<(u32, &'a KotlinField)>> {
    let has_named_fields = info
        .struct_field_names
        .keys()
        .any(|(named_type_index, _)| *named_type_index == type_index);
    fields
        .iter()
        .enumerate()
        .map(|(ordinal, field)| {
            let field_index = if has_named_fields {
                let field_indices = info
                    .struct_field_names
                    .get(&(type_index, field.name.clone()))
                    .with_context(|| {
                        format!(
                            "Kotlin rewrite field {} was not found in WasmGC field names",
                            field.name
                        )
                    })?;
                ensure!(
                    field_indices.len() == 1,
                    "ambiguous Kotlin rewrite field name {} on type index {}: {:?}",
                    field.name,
                    type_index,
                    field_indices
                );
                *field_indices.iter().next().unwrap()
            } else {
                u32::try_from(ordinal).context("Kotlin sidecar field ordinal overflow")?
            };
            Ok((field_index, field))
        })
        .collect()
}

fn validate_sidecar_field_type(
    actual: StorageType,
    field: &KotlinField,
    context: &str,
    info: &GcTypeInfo,
) -> Result<()> {
    match field.kind {
        KotlinFieldKind::I31 => match actual {
            StorageType::Val(ParserValType::Ref(ref_type))
                if matches!(
                    ref_type.heap_type(),
                    HeapType::Abstract {
                        ty: AbstractHeapType::I31,
                        ..
                    }
                ) && ref_type.is_nullable() == field.nullable =>
            {
                Ok(())
            }
            _ => bail!("{context} type mismatch: expected i31, found {actual}"),
        },
        KotlinFieldKind::I32 => validate_scalar_field(actual, ParserValType::I32, context, "i32"),
        KotlinFieldKind::I64 => validate_scalar_field(actual, ParserValType::I64, context, "i64"),
        KotlinFieldKind::F32 => validate_scalar_field(actual, ParserValType::F32, context, "f32"),
        KotlinFieldKind::F64 => validate_scalar_field(actual, ParserValType::F64, context, "f64"),
        KotlinFieldKind::V128 => {
            validate_scalar_field(actual, ParserValType::V128, context, "v128")
        }
        KotlinFieldKind::Ref => validate_ref_field(actual, field, context, info),
    }
}

fn validate_scalar_field(
    actual: StorageType,
    expected: ParserValType,
    context: &str,
    expected_name: &str,
) -> Result<()> {
    if actual == StorageType::Val(expected) {
        Ok(())
    } else {
        bail!("{context} type mismatch: expected {expected_name}, found {actual}")
    }
}

fn validate_ref_field(
    actual: StorageType,
    field: &KotlinField,
    context: &str,
    info: &GcTypeInfo,
) -> Result<()> {
    let StorageType::Val(ParserValType::Ref(ref_type)) = actual else {
        bail!("{context} type mismatch: expected ref, found {actual}");
    };

    if ref_type.is_nullable() != field.nullable {
        bail!(
            "{context} nullability mismatch: sidecar has nullable={}, WasmGC type has nullable={}",
            field.nullable,
            ref_type.is_nullable()
        );
    }

    let actual_type_index = ref_type
        .type_index()
        .and_then(|index| index.unpack().as_module_index())
        .with_context(|| format!("{context} type mismatch: expected concrete persistent ref"))?;
    let Some(target_name) = field.r#type.as_deref() else {
        bail!("{context} is missing ref target type");
    };
    let Some(expected_type_index) = info.sidecar_type_indices.get(target_name).copied() else {
        bail!("{context} references unknown persistent type: {target_name}");
    };

    if !info.persistent.structs.contains(&actual_type_index)
        && !info.persistent.arrays.contains(&actual_type_index)
    {
        bail!(
            "{context} points at non-persistent GC type index {}",
            actual_type_index
        );
    }

    if actual_type_index != expected_type_index {
        bail!(
            "{context} type mismatch: expected ref to {}, found persistent GC type index {}",
            target_name,
            actual_type_index
        );
    }

    Ok(())
}
