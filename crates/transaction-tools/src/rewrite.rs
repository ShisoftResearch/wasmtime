use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use wasm_encoder::reencode::{Error as ReencodeError, Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, CustomSection, Encode, Function, ImportSection, Instruction, Module, RawSection,
};
use wasmparser::{BinaryReader, CodeSectionReader, Operator, Parser, Payload, TypeRef};

const INTRINSIC_MODULE: &str = "twasm_intrinsics";
const PERSISTENT_ADDR_MUT: &str = "__twasm_persistent_addr_mut";
const MARK_TRANSACTION_FUNC: &str = "__twasm_mark_transaction_func";
const MARK_PERSISTENT_ARG: &str = "__twasm_mark_persistent_arg";
const NAME_CUSTOM_SECTION: &str = "name";
const TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";
const TRANSACTION_OBJECTS_VERSION: u8 = 1;
const PERSISTENT_ADDR_MASK: i64 = 0x0fff_ffff_ffff_ffff;

#[derive(Debug, Default, Serialize, Eq, PartialEq)]
pub struct RewriteReport {
    pub transaction_functions: usize,
    pub persistent_addr_markers: usize,
    pub i32_tloads: usize,
    pub i64_tloads: usize,
    pub f32_tloads: usize,
    pub f64_tloads: usize,
    pub i32_tstores: usize,
    pub i64_tstores: usize,
    pub f32_tstores: usize,
    pub f64_tstores: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntrinsicKind {
    PersistentAddrMut,
    MarkTransactionFunc,
    MarkPersistentArg,
}

#[derive(Clone, Copy, Debug, Default)]
struct FuncSig {
    params: usize,
    results: usize,
}

#[derive(Debug, Default)]
struct ModuleLayout {
    type_sigs: Vec<FuncSig>,
    func_index_map: Vec<Option<u32>>,
    func_sigs: Vec<FuncSig>,
    intrinsic_imports: BTreeMap<u32, IntrinsicKind>,
    transaction_func_markers: BTreeSet<u32>,
    persistent_param_markers: BTreeMap<u32, BTreeSet<u32>>,
    function_return_taints: BTreeMap<u32, Vec<bool>>,
    imported_function_count: u32,
    has_memory_zero: bool,
}

#[derive(Debug, Default)]
struct FunctionMarkers {
    transaction: bool,
    persistent_params: BTreeSet<u32>,
}

struct IndexRemapper {
    func_index_map: Vec<Option<u32>>,
}

#[derive(Debug)]
struct RemapError(String);

impl fmt::Display for RemapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for RemapError {}

impl IndexRemapper {
    fn new(func_index_map: Vec<Option<u32>>) -> Self {
        Self { func_index_map }
    }

    fn remap_function_index(
        &self,
        func: u32,
    ) -> std::result::Result<u32, ReencodeError<RemapError>> {
        self.func_index_map
            .get(func as usize)
            .copied()
            .flatten()
            .ok_or_else(|| {
                ReencodeError::UserError(RemapError(format!(
                    "function index {func} was removed during rewrite"
                )))
            })
    }
}

impl Reencode for IndexRemapper {
    type Error = RemapError;

    fn function_index(
        &mut self,
        func: u32,
    ) -> std::result::Result<u32, ReencodeError<Self::Error>> {
        self.remap_function_index(func)
    }
}

pub fn rewrite_module(input: &[u8]) -> Result<(Vec<u8>, RewriteReport)> {
    let layout = analyze_module(input)?;
    let mut remapper = IndexRemapper::new(layout.func_index_map.clone());
    let mut module = Module::new();
    let mut report = RewriteReport::default();
    let mut transaction_functions = BTreeSet::new();
    let mut next_defined_func = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        let payload = payload.context("failed to parse wasm payload")?;
        match payload {
            Payload::Version {
                encoding: wasmparser::Encoding::Module,
                ..
            } => {}
            Payload::Version { .. } => bail!("unsupported non-core wasm module"),
            Payload::TypeSection(section) => {
                let mut types = wasm_encoder::TypeSection::new();
                remapper.parse_type_section(&mut types, section)?;
                module.section(&types);
            }
            Payload::ImportSection(section) => {
                let mut imports = ImportSection::new();
                rewrite_import_section(&mut remapper, &mut imports, section)?;
                if !imports.is_empty() {
                    module.section(&imports);
                }
            }
            Payload::FunctionSection(section) => {
                let mut functions = wasm_encoder::FunctionSection::new();
                remapper.parse_function_section(&mut functions, section)?;
                module.section(&functions);
            }
            Payload::TableSection(section) => {
                let mut tables = wasm_encoder::TableSection::new();
                remapper.parse_table_section(&mut tables, section)?;
                module.section(&tables);
            }
            Payload::MemorySection(section) => {
                let mut memories = wasm_encoder::MemorySection::new();
                remapper.parse_memory_section(&mut memories, section)?;
                module.section(&memories);
            }
            Payload::TagSection(section) => {
                let mut tags = wasm_encoder::TagSection::new();
                remapper.parse_tag_section(&mut tags, section)?;
                module.section(&tags);
            }
            Payload::GlobalSection(section) => {
                let mut globals = wasm_encoder::GlobalSection::new();
                remapper.parse_global_section(&mut globals, section)?;
                module.section(&globals);
            }
            Payload::ExportSection(section) => {
                let mut exports = wasm_encoder::ExportSection::new();
                remapper.parse_export_section(&mut exports, section)?;
                module.section(&exports);
            }
            Payload::StartSection { func, .. } => {
                module.section(&wasm_encoder::StartSection {
                    function_index: remapper.start_section(func)?,
                });
            }
            Payload::ElementSection(section) => {
                let mut elements = wasm_encoder::ElementSection::new();
                remapper.parse_element_section(&mut elements, section)?;
                module.section(&elements);
            }
            Payload::DataCountSection { count, .. } => {
                module.section(&wasm_encoder::DataCountSection {
                    count: remapper.data_count(count)?,
                });
            }
            Payload::DataSection(section) => {
                let mut data = wasm_encoder::DataSection::new();
                remapper.parse_data_section(&mut data, section)?;
                module.section(&data);
            }
            Payload::CodeSectionStart { range, .. } => {
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;
                let mut code = CodeSection::new();

                for body in section {
                    let old_index = layout.imported_function_count + next_defined_func;
                    let body = body?;
                    let function = rewrite_function_body(
                        &mut remapper,
                        &layout,
                        old_index,
                        &body,
                        &mut report,
                        &mut transaction_functions,
                    )?;
                    code.function(&function);
                    next_defined_func += 1;
                }

                module.section(&code);
            }
            Payload::CodeSectionEntry(_) => {}
            Payload::CustomSection(section) => {
                let name = section.name();
                if name == NAME_CUSTOM_SECTION || name == TRANSACTION_OBJECTS_CUSTOM_SECTION {
                    continue;
                }
                module.section(&CustomSection::from(section));
            }
            Payload::End(_) => {}
            other => {
                let (id, range) = other
                    .as_section()
                    .context("unsupported non-section wasm payload")?;
                let contents = input
                    .get(range.start..range.end)
                    .context("invalid section range")?;
                module.section(&RawSection { id, data: contents });
            }
        }
    }

    if layout.has_memory_zero && needs_transaction_metadata(&report, &transaction_functions) {
        let metadata = encode_transaction_objects(&transaction_functions);
        module.section(&CustomSection {
            name: Cow::Borrowed(TRANSACTION_OBJECTS_CUSTOM_SECTION),
            data: Cow::Owned(metadata),
        });
    }

    Ok((module.finish(), report))
}

fn needs_transaction_metadata(
    report: &RewriteReport,
    transaction_functions: &BTreeSet<u32>,
) -> bool {
    !transaction_functions.is_empty()
        || report.i32_tloads != 0
        || report.i64_tloads != 0
        || report.f32_tloads != 0
        || report.f64_tloads != 0
        || report.i32_tstores != 0
        || report.i64_tstores != 0
        || report.f32_tstores != 0
        || report.f64_tstores != 0
}

fn analyze_module(input: &[u8]) -> Result<ModuleLayout> {
    let mut layout = ModuleLayout::default();
    let mut next_old_func = 0u32;
    let mut next_new_func = 0u32;
    let mut saw_function_section = false;
    let mut memory_count = 0u32;

    for payload in Parser::new(0).parse_all(input) {
        let payload = payload.context("failed to parse wasm payload")?;
        match payload {
            Payload::TypeSection(section) => {
                for ty in section.into_iter_err_on_gc_types() {
                    let ty = ty?;
                    layout.type_sigs.push(FuncSig {
                        params: ty.params().len(),
                        results: ty.results().len(),
                    });
                }
            }
            Payload::ImportSection(section) => {
                for imports in section {
                    let imports = imports?;
                    for_each_import(imports, |module, name, ty| {
                        if let Some(type_index) = function_type_index(ty) {
                            let sig = *layout
                                .type_sigs
                                .get(type_index as usize)
                                .context("function import referenced missing type")?;
                            layout.func_sigs.push(sig);
                            let intrinsic = intrinsic_kind(module, name);
                            if is_intrinsic_module(module) && intrinsic.is_none() {
                                bail!("unsupported twasm_intrinsics function import {name}");
                            }
                            let remove_intrinsic_import = intrinsic.is_some();
                            layout.func_index_map.push(if remove_intrinsic_import {
                                None
                            } else {
                                Some(next_new_func)
                            });
                            if let Some(intrinsic) = intrinsic {
                                layout.intrinsic_imports.insert(next_old_func, intrinsic);
                            }
                            if !remove_intrinsic_import {
                                next_new_func += 1;
                            }
                            next_old_func += 1;
                        } else if matches!(ty, TypeRef::Memory(_)) {
                            memory_count += 1;
                        }
                        Ok(())
                    })?;
                }
                layout.imported_function_count = next_old_func;
            }
            Payload::FunctionSection(section) => {
                saw_function_section = true;
                for ty in section {
                    let ty = ty?;
                    let sig = *layout
                        .type_sigs
                        .get(ty as usize)
                        .context("defined function referenced missing type")?;
                    layout.func_sigs.push(sig);
                    layout.func_index_map.push(Some(next_new_func));
                    next_old_func += 1;
                    next_new_func += 1;
                }
            }
            Payload::MemorySection(section) => {
                memory_count += section.count();
            }
            Payload::CodeSectionStart { count, range, .. } => {
                if saw_function_section && count != (next_old_func - layout.imported_function_count)
                {
                    bail!("function and code section length mismatch");
                }
                let body_bytes = input
                    .get(range.start..range.end)
                    .context("invalid code section range")?;
                let reader = BinaryReader::new(body_bytes, range.start);
                let section = CodeSectionReader::new(reader)?;

                for (defined_index, body) in section.into_iter().enumerate() {
                    let old_index = layout.imported_function_count + defined_index as u32;
                    let markers = function_markers_for_body(&layout, old_index, &body?)?;
                    if markers.transaction {
                        layout.transaction_func_markers.insert(old_index);
                    }
                    if !markers.persistent_params.is_empty() {
                        layout
                            .persistent_param_markers
                            .insert(old_index, markers.persistent_params);
                    }
                }
                recompute_function_return_taints(&mut layout, body_bytes, range.start, count)?;
            }
            _ => {}
        }
    }

    layout.has_memory_zero = memory_count > 0;
    Ok(layout)
}

fn rewrite_import_section(
    remapper: &mut IndexRemapper,
    imports: &mut ImportSection,
    section: wasmparser::ImportSectionReader<'_>,
) -> Result<()> {
    for import_group in section {
        let import_group = import_group?;
        for_each_import(import_group, |module, name, ty| {
            if intrinsic_kind(module, name).is_some() && function_type_index(ty).is_some() {
                return Ok(());
            }
            imports.import(module, name, remapper.entity_type(ty)?);
            Ok(())
        })?;
    }
    Ok(())
}

fn rewrite_function_body(
    remapper: &mut IndexRemapper,
    layout: &ModuleLayout,
    old_index: u32,
    body: &wasmparser::FunctionBody<'_>,
    report: &mut RewriteReport,
    transaction_functions: &mut BTreeSet<u32>,
) -> Result<Function> {
    let sig = *layout
        .func_sigs
        .get(old_index as usize)
        .context("missing function signature for rewritten body")?;
    let locals = collect_locals(body)?;
    let mut function = Function::new(locals.iter().copied());
    let mut local_taints = vec![
        false;
        sig.params
            + locals
                .iter()
                .map(|(count, _)| *count as usize)
                .sum::<usize>()
    ];
    let mut stack = Vec::new();
    let mut marked_transactional = false;
    let operators = body
        .get_operators_reader()?
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let current_new_index = remapper.remap_function_index(old_index)?;

    let mut i = 0usize;
    while i < operators.len() {
        if let Some(param_index) = mark_persistent_arg_index(&operators, i, layout) {
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            local_taints[param_index as usize] = true;
            i += 2;
            continue;
        }

        let op = operators[i].clone();
        match op {
            Operator::Call { function_index } => {
                match layout.intrinsic_imports.get(&function_index) {
                    Some(IntrinsicKind::MarkTransactionFunc) => {
                        if !marked_transactional {
                            marked_transactional = true;
                            report.transaction_functions += 1;
                            transaction_functions.insert(current_new_index);
                        }
                    }
                    Some(IntrinsicKind::PersistentAddrMut) => {
                        pop_taint(&mut stack);
                        function.instruction(&Instruction::I64Const(PERSISTENT_ADDR_MASK));
                        function.instruction(&Instruction::I64And);
                        function.instruction(&Instruction::I32WrapI64);
                        stack.push(true);
                        report.persistent_addr_markers += 1;
                    }
                    Some(IntrinsicKind::MarkPersistentArg) => {
                        bail!(
                            "expected i32.const before __twasm_mark_persistent_arg in function index {old_index}"
                        );
                    }
                    None => {
                        let callee_sig = function_sig(layout, function_index)?;
                        let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                        reject_unallowed_direct_tainted_args(
                            layout,
                            old_index,
                            function_index,
                            &arg_taints,
                        )?;
                        function.instruction(&Instruction::Call(
                            remapper.remap_function_index(function_index)?,
                        ));
                        stack.extend(function_return_taints(
                            layout,
                            function_index,
                            callee_sig.results,
                        ));
                    }
                }
            }
            Operator::ReturnCall { function_index } => {
                if layout.intrinsic_imports.contains_key(&function_index) {
                    bail!(
                        "persistent pointer escapes to unrecognized call in function index {old_index}"
                    );
                }
                let callee_sig = function_sig(layout, function_index)?;
                let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                reject_unallowed_direct_tainted_args(
                    layout,
                    old_index,
                    function_index,
                    &arg_taints,
                )?;
                function.instruction(&Instruction::ReturnCall(
                    remapper.remap_function_index(function_index)?,
                ));
                stack.clear();
            }
            Operator::CallIndirect {
                type_index,
                table_index,
            } => {
                let callee_sig = type_sig(layout, type_index)?;
                let table_index_tainted = pop_taint(&mut stack);
                let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                reject_any_tainted_call_operand(old_index, table_index_tainted, &arg_taints)?;
                function.instruction(&Instruction::CallIndirect {
                    type_index: remapper.type_index(type_index)?,
                    table_index: remapper.table_index(table_index)?,
                });
                for _ in 0..callee_sig.results {
                    stack.push(false);
                }
            }
            Operator::ReturnCallIndirect {
                type_index,
                table_index,
            } => {
                let callee_sig = type_sig(layout, type_index)?;
                let table_index_tainted = pop_taint(&mut stack);
                let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                reject_any_tainted_call_operand(old_index, table_index_tainted, &arg_taints)?;
                function.instruction(&Instruction::ReturnCallIndirect {
                    type_index: remapper.type_index(type_index)?,
                    table_index: remapper.table_index(table_index)?,
                });
                stack.clear();
            }
            Operator::CallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                let callee_ref_tainted = pop_taint(&mut stack);
                let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                reject_any_tainted_call_operand(old_index, callee_ref_tainted, &arg_taints)?;
                function.instruction(&remapper.instruction(op)?);
                for _ in 0..callee_sig.results {
                    stack.push(false);
                }
            }
            Operator::ReturnCallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                let callee_ref_tainted = pop_taint(&mut stack);
                let arg_taints = pop_call_arg_taints(&mut stack, callee_sig.params);
                reject_any_tainted_call_operand(old_index, callee_ref_tainted, &arg_taints)?;
                function.instruction(&remapper.instruction(op)?);
                stack.clear();
            }
            Operator::LocalGet { local_index } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(
                    local_taints
                        .get(local_index as usize)
                        .copied()
                        .unwrap_or(false),
                );
            }
            Operator::LocalSet { local_index } => {
                let value = pop_taint(&mut stack);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value;
                }
                function.instruction(&remapper.instruction(op)?);
            }
            Operator::LocalTee { local_index } => {
                let value = stack.last().copied().unwrap_or(false);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value;
                }
                function.instruction(&remapper.instruction(op)?);
            }
            Operator::I32Const { .. }
            | Operator::I64Const { .. }
            | Operator::F32Const { .. }
            | Operator::F64Const { .. } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(false);
            }
            Operator::I32Add | Operator::I64Add | Operator::F32Add | Operator::F64Add => {
                let rhs = pop_taint(&mut stack);
                let lhs = pop_taint(&mut stack);
                function.instruction(&remapper.instruction(op)?);
                stack.push(lhs || rhs);
            }
            Operator::I32Load { memarg } => {
                rewrite_load(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarLoad::I32,
                )?;
                stack.push(false);
            }
            Operator::I64Load { memarg } => {
                rewrite_load(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarLoad::I64,
                )?;
                stack.push(false);
            }
            Operator::F32Load { memarg } => {
                rewrite_load(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarLoad::F32,
                )?;
                stack.push(false);
            }
            Operator::F64Load { memarg } => {
                rewrite_load(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarLoad::F64,
                )?;
                stack.push(false);
            }
            Operator::I32Store { memarg } => {
                pop_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I32,
                )?;
            }
            Operator::I64Store { memarg } => {
                pop_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I64,
                )?;
            }
            Operator::F32Store { memarg } => {
                pop_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::F32,
                )?;
            }
            Operator::F64Store { memarg } => {
                pop_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::F64,
                )?;
            }
            Operator::End => {
                function.instruction(&remapper.instruction(op)?);
                stack.clear();
            }
            _ => {
                function.instruction(&remapper.instruction(op)?);
                stack.clear();
            }
        }

        i += 1;
    }

    Ok(function)
}

#[derive(Clone, Copy)]
enum ScalarLoad {
    I32,
    I64,
    F32,
    F64,
}

#[derive(Clone, Copy)]
enum ScalarStore {
    I32,
    I64,
    F32,
    F64,
}

fn rewrite_load(
    function: &mut Function,
    remapper: &mut IndexRemapper,
    tainted_address: bool,
    memarg: wasmparser::MemArg,
    report: &mut RewriteReport,
    kind: ScalarLoad,
) -> Result<()> {
    let memarg = remapper.mem_arg(memarg)?;
    if tainted_address {
        match kind {
            ScalarLoad::I32 => {
                function.instruction(&Instruction::I32TLoad { memarg });
                report.i32_tloads += 1;
            }
            ScalarLoad::I64 => {
                function.instruction(&Instruction::I64TLoad { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::F32 => {
                function.instruction(&Instruction::F32TLoad { memarg });
                report.f32_tloads += 1;
            }
            ScalarLoad::F64 => {
                function.instruction(&Instruction::F64TLoad { memarg });
                report.f64_tloads += 1;
            }
        }
    } else {
        match kind {
            ScalarLoad::I32 => function.instruction(&Instruction::I32Load(memarg)),
            ScalarLoad::I64 => function.instruction(&Instruction::I64Load(memarg)),
            ScalarLoad::F32 => function.instruction(&Instruction::F32Load(memarg)),
            ScalarLoad::F64 => function.instruction(&Instruction::F64Load(memarg)),
        };
    }
    Ok(())
}

fn rewrite_store(
    function: &mut Function,
    remapper: &mut IndexRemapper,
    tainted_address: bool,
    memarg: wasmparser::MemArg,
    report: &mut RewriteReport,
    kind: ScalarStore,
) -> Result<()> {
    let memarg = remapper.mem_arg(memarg)?;
    if tainted_address {
        match kind {
            ScalarStore::I32 => {
                function.instruction(&Instruction::I32TStore { memarg });
                report.i32_tstores += 1;
            }
            ScalarStore::I64 => {
                function.instruction(&Instruction::I64TStore { memarg });
                report.i64_tstores += 1;
            }
            ScalarStore::F32 => {
                function.instruction(&Instruction::F32TStore { memarg });
                report.f32_tstores += 1;
            }
            ScalarStore::F64 => {
                function.instruction(&Instruction::F64TStore { memarg });
                report.f64_tstores += 1;
            }
        }
    } else {
        match kind {
            ScalarStore::I32 => function.instruction(&Instruction::I32Store(memarg)),
            ScalarStore::I64 => function.instruction(&Instruction::I64Store(memarg)),
            ScalarStore::F32 => function.instruction(&Instruction::F32Store(memarg)),
            ScalarStore::F64 => function.instruction(&Instruction::F64Store(memarg)),
        };
    }
    Ok(())
}

fn collect_locals(
    body: &wasmparser::FunctionBody<'_>,
) -> Result<Vec<(u32, wasm_encoder::ValType)>> {
    let mut locals = Vec::new();
    for local in body.get_locals_reader()? {
        let (count, ty) = local?;
        locals.push((count, RoundtripReencoder.val_type(ty)?));
    }
    Ok(locals)
}

fn function_markers_for_body(
    layout: &ModuleLayout,
    old_index: u32,
    body: &wasmparser::FunctionBody<'_>,
) -> Result<FunctionMarkers> {
    let sig = *layout
        .func_sigs
        .get(old_index as usize)
        .context("missing function signature for marker scan")?;
    let operators = body
        .get_operators_reader()?
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut markers = FunctionMarkers::default();

    let mut i = 0usize;
    while i < operators.len() {
        if let Some(param_index) = mark_persistent_arg_index(&operators, i, layout) {
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            markers.persistent_params.insert(param_index);
            i += 2;
            continue;
        }

        if let Operator::Call { function_index } = operators[i] {
            if layout.intrinsic_imports.get(&function_index)
                == Some(&IntrinsicKind::MarkTransactionFunc)
            {
                markers.transaction = true;
            }
        }

        i += 1;
    }

    Ok(markers)
}

fn recompute_function_return_taints(
    layout: &mut ModuleLayout,
    body_bytes: &[u8],
    body_offset: usize,
    function_count: u32,
) -> Result<()> {
    for defined_index in 0..function_count {
        let old_index = layout.imported_function_count + defined_index;
        let results = function_sig(layout, old_index)?.results;
        layout
            .function_return_taints
            .entry(old_index)
            .or_insert_with(|| vec![false; results]);
    }

    for _ in 0..=function_count {
        let reader = BinaryReader::new(body_bytes, body_offset);
        let section = CodeSectionReader::new(reader)?;
        let mut changed = false;

        for (defined_index, body) in section.into_iter().enumerate() {
            let old_index = layout.imported_function_count + defined_index as u32;
            let taints = function_return_taints_for_body(layout, old_index, &body?)?;
            let entry = layout
                .function_return_taints
                .get_mut(&old_index)
                .context("missing initialized return-taint entry")?;
            if *entry != taints {
                *entry = taints;
                changed = true;
            }
        }

        if !changed {
            return Ok(());
        }
    }

    bail!("function return taint analysis did not converge")
}

fn function_return_taints_for_body(
    layout: &ModuleLayout,
    old_index: u32,
    body: &wasmparser::FunctionBody<'_>,
) -> Result<Vec<bool>> {
    let sig = function_sig(layout, old_index)?;
    let operators = body
        .get_operators_reader()?
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let locals = collect_locals(body)?;
    let mut local_taints = vec![
        false;
        sig.params
            + locals
                .iter()
                .map(|(count, _)| *count as usize)
                .sum::<usize>()
    ];
    let mut stack = Vec::new();
    let mut return_taints = vec![false; sig.results];
    let mut block_depth = 0usize;

    let mut i = 0usize;
    while i < operators.len() {
        if let Some(param_index) = mark_persistent_arg_index(&operators, i, layout) {
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            local_taints[param_index as usize] = true;
            i += 2;
            continue;
        }

        match operators[i].clone() {
            Operator::Call { function_index } => {
                match layout.intrinsic_imports.get(&function_index) {
                    Some(IntrinsicKind::MarkTransactionFunc) => {}
                    Some(IntrinsicKind::PersistentAddrMut) => {
                        pop_taint(&mut stack);
                        stack.push(true);
                    }
                    Some(IntrinsicKind::MarkPersistentArg) => {
                        bail!(
                            "expected i32.const before __twasm_mark_persistent_arg in function index {old_index}"
                        );
                    }
                    None => {
                        let callee_sig = function_sig(layout, function_index)?;
                        for _ in 0..callee_sig.params {
                            pop_taint(&mut stack);
                        }
                        stack.extend(function_return_taints(
                            layout,
                            function_index,
                            callee_sig.results,
                        ));
                    }
                }
            }
            Operator::ReturnCall { function_index } => {
                let callee_sig = function_sig(layout, function_index)?;
                for _ in 0..callee_sig.params {
                    pop_taint(&mut stack);
                }
                merge_return_taints(
                    &mut return_taints,
                    &function_return_taints(layout, function_index, callee_sig.results),
                );
                stack.clear();
            }
            Operator::CallIndirect { type_index, .. } | Operator::CallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                pop_taint(&mut stack);
                for _ in 0..callee_sig.params {
                    pop_taint(&mut stack);
                }
                stack.extend(std::iter::repeat_n(false, callee_sig.results));
            }
            Operator::ReturnCallIndirect { type_index, .. }
            | Operator::ReturnCallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                pop_taint(&mut stack);
                for _ in 0..callee_sig.params {
                    pop_taint(&mut stack);
                }
                merge_return_taints(&mut return_taints, &vec![false; callee_sig.results]);
                stack.clear();
            }
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                block_depth += 1;
            }
            Operator::Else => {
                stack.clear();
            }
            Operator::LocalGet { local_index } => {
                stack.push(
                    local_taints
                        .get(local_index as usize)
                        .copied()
                        .unwrap_or(false),
                );
            }
            Operator::LocalSet { local_index } => {
                let value = pop_taint(&mut stack);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value;
                }
            }
            Operator::LocalTee { local_index } => {
                let value = stack.last().copied().unwrap_or(false);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value;
                }
            }
            Operator::I32Const { .. }
            | Operator::I64Const { .. }
            | Operator::F32Const { .. }
            | Operator::F64Const { .. } => stack.push(false),
            Operator::I32Add | Operator::I64Add | Operator::F32Add | Operator::F64Add => {
                let rhs = pop_taint(&mut stack);
                let lhs = pop_taint(&mut stack);
                stack.push(lhs || rhs);
            }
            Operator::I32Load { .. }
            | Operator::I64Load { .. }
            | Operator::F32Load { .. }
            | Operator::F64Load { .. } => {
                pop_taint(&mut stack);
                stack.push(false);
            }
            Operator::I32Store { .. }
            | Operator::I64Store { .. }
            | Operator::F32Store { .. }
            | Operator::F64Store { .. } => {
                pop_taint(&mut stack);
                pop_taint(&mut stack);
            }
            Operator::Drop => {
                pop_taint(&mut stack);
            }
            Operator::Return => {
                merge_return_taints(&mut return_taints, &stack_results(&stack, sig.results));
                stack.clear();
            }
            Operator::End => {
                if block_depth == 0 {
                    merge_return_taints(&mut return_taints, &stack_results(&stack, sig.results));
                    stack.clear();
                } else {
                    block_depth -= 1;
                    stack.clear();
                }
            }
            _ => {
                stack.clear();
            }
        }

        i += 1;
    }

    Ok(return_taints)
}

fn intrinsic_kind(module: &str, name: &str) -> Option<IntrinsicKind> {
    if !is_intrinsic_module(module) {
        return None;
    }
    match name {
        PERSISTENT_ADDR_MUT => Some(IntrinsicKind::PersistentAddrMut),
        MARK_TRANSACTION_FUNC => Some(IntrinsicKind::MarkTransactionFunc),
        MARK_PERSISTENT_ARG => Some(IntrinsicKind::MarkPersistentArg),
        _ => None,
    }
}

fn is_intrinsic_module(module: &str) -> bool {
    module == INTRINSIC_MODULE
}

fn function_type_index(ty: TypeRef) -> Option<u32> {
    match ty {
        TypeRef::Func(index) | TypeRef::FuncExact(index) => Some(index),
        _ => None,
    }
}

fn for_each_import(
    imports: wasmparser::Imports<'_>,
    mut f: impl FnMut(&str, &str, TypeRef) -> Result<()>,
) -> Result<()> {
    match imports {
        wasmparser::Imports::Single(_, import) => f(import.module, import.name, import.ty),
        wasmparser::Imports::Compact1 { module, items } => {
            for item in items {
                let item = item?;
                f(module, item.name, item.ty)?;
            }
            Ok(())
        }
        wasmparser::Imports::Compact2 { module, ty, names } => {
            for name in names {
                f(module, name?, ty)?;
            }
            Ok(())
        }
    }
}

fn mark_persistent_arg_index(
    operators: &[Operator<'_>],
    index: usize,
    layout: &ModuleLayout,
) -> Option<u32> {
    let Operator::I32Const { value } = operators.get(index)? else {
        return None;
    };
    let Operator::Call { function_index } = operators.get(index + 1)? else {
        return None;
    };
    if layout.intrinsic_imports.get(function_index) != Some(&IntrinsicKind::MarkPersistentArg) {
        return None;
    }
    u32::try_from(*value).ok()
}

fn type_sig(layout: &ModuleLayout, type_index: u32) -> Result<FuncSig> {
    layout
        .type_sigs
        .get(type_index as usize)
        .copied()
        .context("call referenced missing function type")
}

fn function_sig(layout: &ModuleLayout, function_index: u32) -> Result<FuncSig> {
    layout
        .func_sigs
        .get(function_index as usize)
        .copied()
        .context("missing callee signature")
}

fn pop_call_arg_taints(stack: &mut Vec<bool>, params: usize) -> Vec<bool> {
    let mut arg_taints = vec![false; params];
    for param_index in (0..params).rev() {
        arg_taints[param_index] = pop_taint(stack);
    }
    arg_taints
}

fn function_return_taints(
    layout: &ModuleLayout,
    function_index: u32,
    expected_results: usize,
) -> Vec<bool> {
    let Some(taints) = layout.function_return_taints.get(&function_index) else {
        return vec![false; expected_results];
    };
    if taints.len() == expected_results {
        taints.clone()
    } else {
        vec![false; expected_results]
    }
}

fn stack_results(stack: &[bool], results: usize) -> Vec<bool> {
    if results == 0 {
        return Vec::new();
    }
    let start = stack.len().saturating_sub(results);
    let mut taints = stack[start..].to_vec();
    if taints.len() < results {
        taints.resize(results, false);
    }
    taints
}

fn merge_return_taints(target: &mut Vec<bool>, source: &[bool]) {
    if target.len() < source.len() {
        target.resize(source.len(), false);
    }
    for (target, source) in target.iter_mut().zip(source) {
        *target |= *source;
    }
}

fn reject_unallowed_direct_tainted_args(
    layout: &ModuleLayout,
    caller_index: u32,
    callee_index: u32,
    arg_taints: &[bool],
) -> Result<()> {
    let caller_transactional = layout.transaction_func_markers.contains(&caller_index);
    let callee_transactional = layout.transaction_func_markers.contains(&callee_index);
    let persistent_params = layout.persistent_param_markers.get(&callee_index);
    let has_unmarked_tainted_arg = arg_taints.iter().enumerate().any(|(param_index, tainted)| {
        *tainted
            && !(caller_transactional
                && callee_transactional
                && persistent_params.is_some_and(|params| params.contains(&(param_index as u32))))
    });

    if has_unmarked_tainted_arg {
        bail!("persistent pointer escapes to unrecognized call in function index {caller_index}");
    }

    Ok(())
}

fn reject_any_tainted_call_operand(
    caller_index: u32,
    callee_operand_tainted: bool,
    arg_taints: &[bool],
) -> Result<()> {
    if callee_operand_tainted || arg_taints.iter().any(|tainted| *tainted) {
        bail!("persistent pointer escapes to unrecognized call in function index {caller_index}");
    }

    Ok(())
}

fn pop_taint(stack: &mut Vec<bool>) -> bool {
    stack.pop().unwrap_or(false)
}

fn encode_transaction_objects(transaction_functions: &BTreeSet<u32>) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.push(TRANSACTION_OBJECTS_VERSION);
    1u32.encode(&mut bytes);
    0u32.encode(&mut bytes);
    0u32.encode(&mut bytes);
    (transaction_functions.len() as u32).encode(&mut bytes);
    for function in transaction_functions {
        function.encode(&mut bytes);
    }
    bytes
}

pub fn main() -> anyhow::Result<()> {
    crate::cli::main()
}

#[cfg(test)]
mod tests {
    use super::{RewriteReport, rewrite_module};

    #[test]
    fn rewrite_module_is_noop() {
        let input = b"\0asm\x01\0\0\0";
        let (output, report) = rewrite_module(input).expect("rewrite succeeds");

        assert_eq!(output, input);
        assert_eq!(report, RewriteReport::default());
    }
}
