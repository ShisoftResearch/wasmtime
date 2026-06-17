use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, Encode, Function, Instruction, Module, RawSection, ValType as EncoderValType,
};
use wasmparser::{
    BinaryReader, CodeSectionReader, CompositeInnerType, ExternalKind, Operator, Parser, Payload,
    TypeRef, ValType as ParserValType,
};

use crate::kotlin_metadata::{KotlinPersistentKind, KotlinSidecar, validate_kotlin_sidecar};

const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";
const TRANSACTION_OBJECTS_VERSION: u8 = 1;

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

pub fn rewrite_kotlin_module(
    input: &[u8],
    sidecar: &KotlinSidecar,
) -> Result<(Vec<u8>, KotlinRewriteReport)> {
    validate_kotlin_sidecar(sidecar)?;

    let exported_functions = exported_function_names(input)?;
    let transaction_function_indices = transaction_function_indices(&exported_functions, sidecar)?;
    let gc_type_info = gc_type_info(input, sidecar)?;
    let imported_function_count = imported_function_count(input)?;
    let func_type_params = function_type_params(input)?;
    let defined_function_types = defined_function_type_indices(input)?;

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
                module.section(&globals);
            }
            Payload::ExportSection(section) => {
                let mut exports = wasm_encoder::ExportSection::new();
                reencoder.parse_export_section(&mut exports, section)?;
                module.section(&exports);
            }
            Payload::StartSection { func, .. } => {
                module.section(&wasm_encoder::StartSection {
                    function_index: reencoder.start_section(func)?,
                });
            }
            Payload::ElementSection(section) => {
                let mut elements = wasm_encoder::ElementSection::new();
                reencoder.parse_element_section(&mut elements, section)?;
                module.section(&elements);
            }
            Payload::DataCountSection { count, .. } => {
                module.section(&wasm_encoder::DataCountSection {
                    count: reencoder.data_count(count)?,
                });
            }
            Payload::DataSection(section) => {
                let mut data = wasm_encoder::DataSection::new();
                reencoder.parse_data_section(&mut data, section)?;
                module.section(&data);
            }
            Payload::CodeSectionStart { range, .. } => {
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
                        transaction_function_indices.contains(&function_index),
                        &gc_type_info,
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

    if !transaction_function_indices.is_empty() {
        module.section(&wasm_encoder::CustomSection {
            name: Cow::Borrowed(TRANSACTION_OBJECTS_CUSTOM_SECTION),
            data: Cow::Owned(encode_transaction_objects(&transaction_function_indices)),
        });
    }

    Ok((module.finish(), report))
}

fn rewrite_function_body(
    body: &wasmparser::FunctionBody<'_>,
    rewrite_object_ops: bool,
    gc_type_info: &GcTypeInfo,
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

    while !reader.eof() {
        let op = reader.read()?;
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

fn heap_type_index(heap_type: wasmparser::HeapType) -> Option<u32> {
    match heap_type {
        wasmparser::HeapType::Concrete(index) | wasmparser::HeapType::Exact(index) => {
            index.as_module_index()
        }
        _ => None,
    }
}

fn encode_transaction_objects(transaction_functions: &BTreeSet<u32>) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(TRANSACTION_OBJECTS_VERSION);
    0u32.encode(&mut bytes);
    0u32.encode(&mut bytes);
    (transaction_functions.len() as u32).encode(&mut bytes);
    for function in transaction_functions {
        function.encode(&mut bytes);
    }
    0u32.encode(&mut bytes);
    bytes
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

fn transaction_function_indices(
    exported_functions: &BTreeMap<String, u32>,
    sidecar: &KotlinSidecar,
) -> Result<BTreeSet<u32>> {
    let mut indices = BTreeSet::new();

    for name in &sidecar.transaction_functions {
        let Some(index) = exported_functions.get(name) else {
            bail!("transaction function {name} was not found");
        };
        indices.insert(*index);
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

    for payload in Parser::new(0).parse_all(input) {
        match payload.context("failed to parse Kotlin rewrite type payload")? {
            Payload::TypeSection(section) => {
                for group in section {
                    let group = group.context("failed to parse Kotlin rewrite type group")?;
                    for ty in group.into_types() {
                        match ty.composite_type.inner {
                            CompositeInnerType::Struct(struct_ty) => {
                                for (field_index, field) in struct_ty.fields.iter().enumerate() {
                                    info.struct_fields.insert(
                                        (next_type_index, field_index as u32),
                                        field.element_type.unpack(),
                                    );
                                }
                                gc_type_indices
                                    .push((next_type_index, KotlinPersistentKind::Struct));
                            }
                            CompositeInnerType::Array(array_ty) => {
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

    for (ordinal, persistent_type) in sidecar.persistent_types.iter().enumerate() {
        let Some((type_index, actual_kind)) = gc_type_indices.get(ordinal).copied() else {
            bail!(
                "persistent type {} could not be mapped to GC type ordinal {}",
                persistent_type.name,
                ordinal
            );
        };

        if actual_kind != persistent_type.kind {
            bail!(
                "persistent type {} expected {:?} at GC type ordinal {}, found {:?}",
                persistent_type.name,
                persistent_type.kind,
                ordinal,
                actual_kind
            );
        }

        match persistent_type.kind {
            KotlinPersistentKind::Struct => {
                info.persistent.structs.insert(type_index);
            }
            KotlinPersistentKind::Array => {
                info.persistent.arrays.insert(type_index);
            }
        }
    }

    Ok(info)
}
