use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, ConstExpr, Encode, Function, GlobalType as EncoderGlobalType,
    HeapType as EncoderHeapType, Instruction, Module, RawSection, RefType as EncoderRefType,
    TransactionRefPermission as EncoderTransactionRefPermission, ValType as EncoderValType,
};
use wasmparser::{
    AbstractHeapType, BinaryReader, BlockType, CodeSectionReader, CompositeInnerType, ExternalKind,
    HeapType, KnownCustom, Name, Operator, Parser, Payload, StorageType, TypeRef,
    ValType as ParserValType,
};

use crate::kotlin_metadata::{
    KotlinField, KotlinFieldKind, KotlinPersistentKind, KotlinPersistentType, KotlinSidecar,
    validate_kotlin_sidecar,
};

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";
const TRANSACTION_OBJECTS_VERSION: u8 = 1;
const KOTLIN_RUNTIME_STRUCT_FIELDS: &[&str] = &["vtable", "itable", "rtti", "_hashCode"];

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
    sidecar_type_indices: BTreeMap<String, u32>,
    struct_field_counts: BTreeMap<u32, usize>,
    struct_field_names: BTreeMap<(u32, String), BTreeSet<u32>>,
    struct_field_storage: BTreeMap<(u32, u32), StorageType>,
    array_element_storage: BTreeMap<u32, StorageType>,
    struct_fields: BTreeMap<(u32, u32), ParserValType>,
    array_elements: BTreeMap<u32, ParserValType>,
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
}

impl TransactionObjects {
    fn is_empty(&self) -> bool {
        self.memories.is_empty()
            && self.globals.is_empty()
            && self.functions.is_empty()
            && self.tables.is_empty()
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

    Ok(RootLowering {
        globals,
        unit_getter_func,
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
    let mut object_rewrite_function_indices = transaction_function_indices.clone();
    object_rewrite_function_indices.extend(persistent_accessor_function_indices(input, sidecar)?);
    let mut transaction_objects = transaction_objects(input)?;
    transaction_objects
        .functions
        .extend(transaction_function_indices.iter().copied());
    let gc_type_info = gc_type_info(input, sidecar)?;
    let root_lowering = root_lowering(input, sidecar, &gc_type_info)?;
    transaction_objects
        .globals
        .extend(root_lowering.globals.iter().map(|root| root.global_index));
    let imported_function_count = imported_function_count(input)?;
    let func_type_params = function_type_params(input)?;
    let defined_function_types = defined_function_type_indices(input)?;
    let root_marker_function_indices = root_marker_function_indices(
        input,
        &root_lowering,
        imported_function_count,
        &func_type_params,
        &defined_function_types,
    )?;
    transaction_objects
        .functions
        .extend(root_marker_function_indices.iter().copied());
    object_rewrite_function_indices.extend(root_marker_function_indices.iter().copied());

    let mut report = KotlinRewriteReport {
        transaction_functions: sidecar.transaction_functions.clone(),
        persistent_types: sidecar.persistent_types.len(),
        roots: sidecar.roots.len(),
        rewritten_tfuncs: transaction_function_indices.len(),
        rewritten_object_ops: 0,
    };

    let mut reencoder = RoundtripReencoder;
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
                reencoder.parse_type_section(&mut types, section)?;
                module.section(&types);
            }
            Payload::ImportSection(section) => {
                let mut imports = wasm_encoder::ImportSection::new();
                reencoder.parse_import_section(&mut imports, section)?;
                module.section(&imports);
            }
            Payload::FunctionSection(section) => {
                let mut functions = wasm_encoder::FunctionSection::new();
                reencoder.parse_function_section(&mut functions, section)?;
                module.section(&functions);
            }
            Payload::TableSection(section) => {
                let mut tables = wasm_encoder::TableSection::new();
                reencoder.parse_table_section(&mut tables, section)?;
                module.section(&tables);
            }
            Payload::MemorySection(section) => {
                let mut memories = wasm_encoder::MemorySection::new();
                reencoder.parse_memory_section(&mut memories, section)?;
                module.section(&memories);
            }
            Payload::TagSection(section) => {
                let mut tags = wasm_encoder::TagSection::new();
                reencoder.parse_tag_section(&mut tags, section)?;
                module.section(&tags);
            }
            Payload::GlobalSection(section) => {
                let mut globals = wasm_encoder::GlobalSection::new();
                reencoder.parse_global_section(&mut globals, section)?;
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
                reencoder.parse_export_section(&mut exports, section)?;
                module.section(&exports);
            }
            Payload::StartSection { func, .. } => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                module.section(&wasm_encoder::StartSection {
                    function_index: reencoder.start_section(func)?,
                });
            }
            Payload::ElementSection(section) => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let mut elements = wasm_encoder::ElementSection::new();
                reencoder.parse_element_section(&mut elements, section)?;
                module.section(&elements);
            }
            Payload::DataCountSection { count, .. } => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                module.section(&wasm_encoder::DataCountSection {
                    count: reencoder.data_count(count)?,
                });
            }
            Payload::DataSection(section) => {
                emit_root_global_section_if_needed(
                    &mut module,
                    &root_lowering,
                    &mut emitted_global_section,
                );
                let mut data = wasm_encoder::DataSection::new();
                reencoder.parse_data_section(&mut data, section)?;
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
                        &params,
                        &mut report,
                    )?;
                    code.function(&function);
                    next_defined_func += 1;
                }

                module.section(&code);
            }
            Payload::CodeSectionEntry(_) => {}
            Payload::CustomSection(section) => {
                if section.name() != TRANSACTION_OBJECTS_CUSTOM_SECTION {
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
    params: &[ParserValType],
    report: &mut KotlinRewriteReport,
) -> Result<Function> {
    let mut reencoder = RoundtripReencoder;
    let local_types = local_types(body, params)?;
    let required_temps = required_temps(body, rewrite_object_ops, gc_type_info)?;
    let (local_decls, temp_locals) =
        local_decls_with_temps(&mut reencoder, body, &local_types, required_temps)?;
    let mut function = Function::new(local_decls);
    let mut type_stack = Vec::new();
    let mut reader = body.get_operators_reader()?;
    let mut operators = Vec::new();

    while !reader.eof() {
        operators.push(reader.read()?);
    }

    let mut index = 0usize;
    while index < operators.len() {
        if let Some(marker) = match_inline_set_root_marker(&operators, index, &local_types)? {
            if let Some(root) = root_for_marker_type(root_lowering, marker.type_index)? {
                let unit_getter = root_lowering
                    .unit_getter_func
                    .context("Kotlin setRoot marker lowering requires kotlin.Unit_getInstance")?;
                function.instruction(&Instruction::LocalGet(marker.value_local));
                function.instruction(&Instruction::TGlobalSet {
                    global_index: root.global_index,
                });
                function.instruction(&Instruction::Call(unit_getter));
                type_stack.push(None);
                index = marker.next_index;
                continue;
            }
        }

        if let Some(marker) = match_inline_get_root_marker(&operators, index)?
            && let Some(root) = root_for_marker_type(root_lowering, marker.type_index)?
        {
            function.instruction(&Instruction::TGlobalGet {
                global_index: root.global_index,
            });
            type_stack.push(Some(root.type_index));
            index = marker.next_index;
            continue;
        }

        let op = operators[index].clone();
        let is_array_len = matches!(op, Operator::ArrayLen);
        let array_len_operand = if is_array_len {
            type_stack.pop().flatten()
        } else {
            update_type_stack_for_operator(&op, &local_types, &mut type_stack);
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
                type_stack.push(None);
            }
            _ => {
                function.instruction(&reencoder.instruction(op)?);
                if is_array_len {
                    type_stack.push(None);
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

// SHISOFT-TWASM-MOCK: this recognizes the current Kotlin SDK inline
// root/setRoot throw-marker shape. Replace it with explicit SDK imports or
// compiler-emitted intrinsics once the Kotlin frontend contract is stabilized.
fn match_inline_set_root_marker(
    operators: &[Operator<'_>],
    index: usize,
    local_types: &[ParserValType],
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
    if !body_contains_i32_const(body, 658) || !body_ends_with_throw_unreachable(body) {
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
            Operator::I32Const { value: 658 },
        ] => {
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
    if body_contains_i32_const(body, 659) && body_ends_with_throw_unreachable(body) {
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

fn body_contains_i32_const(operators: &[Operator<'_>], value: i32) -> bool {
    operators
        .iter()
        .any(|op| matches!(op, Operator::I32Const { value: candidate } if *candidate == value))
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

fn update_type_stack_for_operator(
    op: &Operator<'_>,
    local_types: &[ParserValType],
    type_stack: &mut Vec<Option<u32>>,
) {
    match op {
        Operator::LocalGet { local_index } => {
            let type_index = local_types
                .get(*local_index as usize)
                .copied()
                .and_then(module_ref_type_index);
            type_stack.push(type_index);
        }
        Operator::Drop => {
            type_stack.pop();
        }
        Operator::LocalSet { .. } => {
            type_stack.pop();
        }
        Operator::LocalTee { local_index } => {
            type_stack.pop();
            let type_index = local_types
                .get(*local_index as usize)
                .copied()
                .and_then(module_ref_type_index);
            type_stack.push(type_index);
        }
        Operator::I32Const { .. }
        | Operator::I64Const { .. }
        | Operator::F32Const { .. }
        | Operator::F64Const { .. }
        | Operator::V128Const { .. } => {
            type_stack.push(None);
        }
        Operator::StructGet { .. } | Operator::StructGetS { .. } | Operator::StructGetU { .. } => {
            type_stack.pop();
            type_stack.push(None);
        }
        Operator::StructSet { .. } => {
            pop_n(type_stack, 2);
        }
        Operator::ArrayGet { .. } | Operator::ArrayGetS { .. } | Operator::ArrayGetU { .. } => {
            pop_n(type_stack, 2);
            type_stack.push(None);
        }
        Operator::ArraySet { .. } => {
            pop_n(type_stack, 3);
        }
        Operator::RefNull { hty } => {
            type_stack.push(heap_type_index(*hty));
        }
        _ => {
            type_stack.clear();
        }
    }
}

fn pop_n(type_stack: &mut Vec<Option<u32>>, count: usize) {
    for _ in 0..count {
        type_stack.pop();
    }
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
    let mut count = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite import payload")? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin rewrite import")?;
                    if matches!(import.ty, TypeRef::Func(_) | TypeRef::FuncExact(_)) {
                        count += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(count)
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
    let mut function_entries = exported_function_names(input)?
        .into_iter()
        .map(|(name, index)| (name, index))
        .collect::<Vec<_>>();
    function_entries.extend(name_section_function_names(input)?);

    let matches = function_entries
        .iter()
        .filter_map(|(name, index)| (name == expected).then_some(*index))
        .collect::<BTreeSet<_>>();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(*matches.iter().next().unwrap())),
        _ => bail!("ambiguous Kotlin rewrite function name {expected}: {matches:?}"),
    }
}

fn root_marker_function_indices(
    input: &[u8],
    root_lowering: &RootLowering,
    imported_function_count: u32,
    func_type_params: &BTreeMap<u32, Vec<ParserValType>>,
    defined_function_types: &[u32],
) -> Result<BTreeSet<u32>> {
    let mut indices = BTreeSet::new();
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
                    if function_contains_inline_root_marker(&body, &params, root_lowering)? {
                        indices.insert(function_index);
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

fn function_contains_inline_root_marker(
    body: &wasmparser::FunctionBody<'_>,
    params: &[ParserValType],
    root_lowering: &RootLowering,
) -> Result<bool> {
    let local_types = local_types(body, params)?;
    let mut reader = body.get_operators_reader()?;
    let mut operators = Vec::new();
    while !reader.eof() {
        operators.push(reader.read()?);
    }

    for index in 0..operators.len() {
        if let Some(marker) = match_inline_set_root_marker(&operators, index, &local_types)? {
            if root_for_marker_type(root_lowering, marker.type_index)?.is_some() {
                return Ok(true);
            }
        }
        if let Some(marker) = match_inline_get_root_marker(&operators, index)?
            && root_for_marker_type(root_lowering, marker.type_index)?.is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn function_type_params(input: &[u8]) -> Result<BTreeMap<u32, Vec<ParserValType>>> {
    let mut params = BTreeMap::new();
    let mut next_type_index = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite type payload")? {
            Payload::TypeSection(section) => {
                for group in section {
                    let group = group.context("failed to parse Kotlin rewrite type group")?;
                    for ty in group.into_types() {
                        if let CompositeInnerType::Func(func) = ty.composite_type.inner {
                            params.insert(next_type_index, func.params().to_vec());
                        }
                        next_type_index += 1;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(params)
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
            _ => {}
        }
    }

    let mut mapped_type_indices = BTreeSet::new();
    let mut fallback_ordinal = 0usize;
    for persistent_type in &sidecar.persistent_types {
        let (type_index, actual_kind) = if let Some(type_indices) =
            type_names.get(&persistent_type.name)
        {
            if type_indices.len() != 1 {
                bail!(
                    "ambiguous Kotlin rewrite type name {}: {:?}",
                    persistent_type.name,
                    type_indices
                );
            }
            let type_index = *type_indices.iter().next().unwrap();
            let actual_kind = if info.struct_field_counts.contains_key(&type_index) {
                KotlinPersistentKind::Struct
            } else if info.array_element_storage.contains_key(&type_index) {
                KotlinPersistentKind::Array
            } else {
                bail!(
                    "persistent type {} maps to non-GC type index {}",
                    persistent_type.name,
                    type_index
                );
            };
            let gc_position = gc_type_indices
                .iter()
                .position(|(candidate, _)| *candidate == type_index)
                .context("named persistent type did not map to a GC type")?;
            fallback_ordinal = fallback_ordinal.max(gc_position + 1);
            (type_index, actual_kind)
        } else {
            while gc_type_indices
                .get(fallback_ordinal)
                .is_some_and(|(candidate, _)| mapped_type_indices.contains(candidate))
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
            }
            KotlinPersistentKind::Array => {
                info.persistent.arrays.insert(type_index);
            }
        }
    }

    for persistent_type in &sidecar.persistent_types {
        let type_index = info.sidecar_type_indices[&persistent_type.name];
        validate_persistent_type_shape(type_index, persistent_type, &info)?;
    }

    Ok(info)
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
