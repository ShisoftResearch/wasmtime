use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use wasm_encoder::reencode::{Error as ReencodeError, Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, CustomSection, DataSegment, DataSegmentMode, EntityNamespace, Function,
    ImportSection, Instruction, Module, RawSection, ValType,
};
use wasmparser::{BinaryReader, CodeSectionReader, Operator, Parser, Payload, TypeRef};

const INTRINSIC_MODULE: &str = "twasm_intrinsics";
const PERSISTENT_ADDR_MUT: &str = "__twasm_persistent_addr_mut";
const MARK_TRANSACTION_FUNC: &str = "__twasm_mark_transaction_func";
const MARK_PERSISTENT_ARG: &str = "__twasm_mark_persistent_arg";
const NAME_CUSTOM_SECTION: &str = "name";
const OBSOLETE_TRANSACTION_OBJECTS_CUSTOM_SECTION: &str = "shisoft.transaction.objects";
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
    encoded_func_types: Vec<wasm_encoder::FuncType>,
    func_index_map: Vec<Option<u32>>,
    func_sigs: Vec<FuncSig>,
    func_type_indices: Vec<u32>,
    intrinsic_imports: BTreeMap<u32, IntrinsicKind>,
    transaction_func_markers: BTreeSet<u32>,
    persistent_param_markers: BTreeMap<u32, BTreeSet<u32>>,
    function_return_taints: BTreeMap<u32, Vec<bool>>,
    function_param_store_taints: BTreeMap<u32, BTreeMap<ParamStoreSlot, bool>>,
    imported_function_count: u32,
    memory_count: u32,
    has_imported_memory: bool,
}

#[derive(Debug, Default)]
struct FunctionMarkers {
    transaction: bool,
    persistent_params: BTreeSet<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PersistentArgMarker {
    ImmediateConst(u32),
    SpilledConst(u32),
}

impl PersistentArgMarker {
    fn param_index(self) -> u32 {
        match self {
            Self::ImmediateConst(param_index) | Self::SpilledConst(param_index) => param_index,
        }
    }
}

struct IndexRemapper {
    func_index_map: Vec<Option<u32>>,
    transaction_type_indices: BTreeMap<u32, u32>,
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
    fn new(func_index_map: Vec<Option<u32>>, transaction_type_indices: BTreeMap<u32, u32>) -> Self {
        Self {
            func_index_map,
            transaction_type_indices,
        }
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
    let native_rewrite = !layout.transaction_func_markers.is_empty()
        || layout
            .intrinsic_imports
            .values()
            .any(|kind| matches!(kind, IntrinsicKind::PersistentAddrMut));
    if native_rewrite && layout.has_imported_memory {
        bail!("native transaction rewrite does not support an imported source memory");
    }
    if native_rewrite && layout.memory_count != 1 {
        bail!(
            "native transaction rewrite requires exactly one source memory, found {}",
            layout.memory_count
        );
    }
    let mut transaction_source_types = BTreeSet::new();
    for function in &layout.transaction_func_markers {
        let ty = *layout
            .func_type_indices
            .get(*function as usize)
            .context("transaction function referenced missing type")?;
        transaction_source_types.insert(ty);
    }
    let mut transaction_type_indices = BTreeMap::new();
    for (ordinal, ty) in transaction_source_types.into_iter().enumerate() {
        let next = u32::try_from(layout.type_sigs.len() + ordinal)
            .context("transaction function type index overflow")?;
        transaction_type_indices.insert(ty, next);
    }
    let mut remapper = IndexRemapper::new(layout.func_index_map.clone(), transaction_type_indices);
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
                for (source, transaction) in &remapper.transaction_type_indices {
                    let ty = layout
                        .encoded_func_types
                        .get(*source as usize)
                        .context("transaction function referenced non-function type")?;
                    debug_assert_eq!(u32::try_from(types.len()).unwrap(), *transaction);
                    let ty = wasm_encoder::FuncType::new_with_transaction(
                        ty.params().iter().copied(),
                        ty.results().iter().copied(),
                        true,
                    );
                    types.ty().func_type(&ty);
                }
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
                for (defined, ty) in section.into_iter().enumerate() {
                    let ty = ty?;
                    let old = layout.imported_function_count
                        + u32::try_from(defined).context("defined function index overflow")?;
                    let ty = if layout.transaction_func_markers.contains(&old) {
                        *remapper
                            .transaction_type_indices
                            .get(&ty)
                            .context("missing native transaction function type")?
                    } else {
                        remapper.type_index(ty)?
                    };
                    functions.function(ty);
                }
                module.section(&functions);
            }
            Payload::TableSection(section) => {
                let mut tables = wasm_encoder::TableSection::new();
                remapper.parse_table_section(&mut tables, section)?;
                module.section(&tables);
            }
            Payload::MemorySection(section) => {
                let mut memories = wasm_encoder::MemorySection::new();
                let transactional = section.clone();
                remapper.parse_memory_section(&mut memories, section)?;
                if native_rewrite {
                    for memory in transactional {
                        let mut ty = remapper.memory_type(memory?)?;
                        ty.namespace = EntityNamespace::Transactional;
                        memories.memory(ty);
                    }
                }
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
                    count: remapper.data_count(if native_rewrite {
                        count.checked_mul(2).context("native data count overflow")?
                    } else {
                        count
                    })?,
                });
            }
            Payload::DataSection(section) => {
                let mut data = wasm_encoder::DataSection::new();
                let transactional = section.clone();
                remapper.parse_data_section(&mut data, section)?;
                if native_rewrite {
                    for datum in transactional {
                        let datum = datum?;
                        match datum.kind {
                            wasmparser::DataKind::Active {
                                memory_index,
                                offset_expr,
                            } => {
                                let offset = remapper.const_expr(offset_expr)?;
                                data.segment(DataSegment {
                                    namespace: EntityNamespace::Transactional,
                                    mode: DataSegmentMode::Active {
                                        memory_index: remapper.memory_index(memory_index)?,
                                        offset: &offset,
                                    },
                                    data: datum.data.iter().copied(),
                                });
                            }
                            wasmparser::DataKind::ActiveWithMemoryIndex {
                                memory_index,
                                offset_expr,
                            } => {
                                let offset = remapper.const_expr(offset_expr)?;
                                data.segment(DataSegment {
                                    namespace: EntityNamespace::Transactional,
                                    mode: DataSegmentMode::ActiveWithMemoryIndex {
                                        memory_index: remapper.memory_index(memory_index)?,
                                        offset: &offset,
                                    },
                                    data: datum.data.iter().copied(),
                                });
                            }
                            wasmparser::DataKind::Passive => {
                                data.segment(DataSegment {
                                    namespace: EntityNamespace::Transactional,
                                    mode: DataSegmentMode::Passive,
                                    data: datum.data.iter().copied(),
                                });
                            }
                        }
                    }
                }
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
                if name == OBSOLETE_TRANSACTION_OBJECTS_CUSTOM_SECTION {
                    bail!("obsolete shisoft.transaction.objects metadata is not accepted");
                }
                if name == NAME_CUSTOM_SECTION {
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

    Ok((module.finish(), report))
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
                let mut reencoder = RoundtripReencoder;
                for ty in section.into_iter_err_on_gc_types() {
                    let ty = ty?;
                    layout
                        .encoded_func_types
                        .push(reencoder.func_type(ty.clone())?);
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
                            layout.func_type_indices.push(type_index);
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
                            layout.has_imported_memory = true;
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
                    layout.func_type_indices.push(ty);
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

    layout.memory_count = memory_count;
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
    let operators = body
        .get_operators_reader()?
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let original_local_count = locals
        .iter()
        .map(|(count, _)| *count as usize)
        .sum::<usize>();
    let memory_copy_scratch = operators
        .iter()
        .any(|op| matches!(op, Operator::MemoryCopy { .. }))
        .then(|| {
            let first = sig
                .params
                .checked_add(original_local_count)
                .context("function local count overflow")?;
            let first =
                u32::try_from(first).context("function local count does not fit wasm index")?;
            Ok::<_, anyhow::Error>(MemoryCopyScratch {
                dst: first,
                src: first + 1,
                len: first + 2,
            })
        })
        .transpose()?;
    let mut function_locals = locals.clone();
    if memory_copy_scratch.is_some() {
        function_locals.push((3, ValType::I32));
    }
    let mut function = Function::new(function_locals);
    let mut local_taints =
        vec![
            false;
            sig.params + original_local_count + usize::from(memory_copy_scratch.is_some()) * 3
        ];
    if let Some(params) = layout.persistent_param_markers.get(&old_index) {
        for param in params {
            if (*param as usize) < sig.params {
                local_taints[*param as usize] = true;
            }
        }
    }
    let mut stack = Vec::<AnalysisValue>::new();
    let mut spilled_taints = BTreeMap::<AnalysisStackSlot, bool>::new();
    let mut marked_transactional = false;
    let current_new_index = remapper.remap_function_index(old_index)?;

    let mut i = 0usize;
    while i < operators.len() {
        if let Some(marker) = mark_persistent_arg_marker(&operators, i, layout) {
            let param_index = marker.param_index();
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            local_taints[param_index as usize] = true;
            if marker == PersistentArgMarker::SpilledConst(param_index) {
                pop_analysis_taint(&mut stack);
                function.instruction(&Instruction::Drop);
                i += 1;
            } else {
                i += 2;
            }
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
                        pop_analysis_taint(&mut stack);
                        function.instruction(&Instruction::I64Const(PERSISTENT_ADDR_MASK));
                        function.instruction(&Instruction::I64And);
                        function.instruction(&Instruction::I32WrapI64);
                        stack.push(AnalysisValue::tainted());
                        report.persistent_addr_markers += 1;
                    }
                    Some(IntrinsicKind::MarkPersistentArg) => {
                        bail!(
                            "expected i32.const before __twasm_mark_persistent_arg in function index {old_index}"
                        );
                    }
                    None => {
                        let callee_sig = function_sig(layout, function_index)?;
                        let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                        let arg_taints = analysis_arg_taints(&args);
                        if !call_is_immediately_unreachable(&operators, i) {
                            reject_unallowed_direct_tainted_args(
                                layout,
                                old_index,
                                function_index,
                                &arg_taints,
                            )?;
                        }
                        function.instruction(&Instruction::Call(
                            remapper.remap_function_index(function_index)?,
                        ));
                        apply_callee_param_store_taints(
                            layout,
                            function_index,
                            &args,
                            &mut spilled_taints,
                        );
                        push_analysis_taints(
                            &mut stack,
                            function_return_taints(layout, function_index, callee_sig.results),
                        );
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
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
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
                table_namespace,
                flags,
            } => {
                let callee_sig = type_sig(layout, type_index)?;
                let table_index_tainted = pop_analysis_taint(&mut stack);
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
                reject_any_tainted_call_operand(old_index, table_index_tainted, &arg_taints)?;
                function.instruction(&Instruction::CallIndirect {
                    type_index: remapper.type_index(type_index)?,
                    table_index: remapper.table_index(table_index)?,
                    table_namespace: match table_namespace {
                        wasmparser::EntityNamespace::Ordinary => {
                            wasm_encoder::EntityNamespace::Ordinary
                        }
                        wasmparser::EntityNamespace::Transactional => {
                            wasm_encoder::EntityNamespace::Transactional
                        }
                    },
                    flags,
                });
                for _ in 0..callee_sig.results {
                    stack.push(AnalysisValue::untainted());
                }
            }
            Operator::ReturnCallIndirect {
                type_index,
                table_index,
                table_namespace,
                flags,
            } => {
                let callee_sig = type_sig(layout, type_index)?;
                let table_index_tainted = pop_analysis_taint(&mut stack);
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
                reject_any_tainted_call_operand(old_index, table_index_tainted, &arg_taints)?;
                function.instruction(&Instruction::ReturnCallIndirect {
                    type_index: remapper.type_index(type_index)?,
                    table_index: remapper.table_index(table_index)?,
                    table_namespace: match table_namespace {
                        wasmparser::EntityNamespace::Ordinary => {
                            wasm_encoder::EntityNamespace::Ordinary
                        }
                        wasmparser::EntityNamespace::Transactional => {
                            wasm_encoder::EntityNamespace::Transactional
                        }
                    },
                    flags,
                });
                stack.clear();
            }
            Operator::CallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                let callee_ref_tainted = pop_analysis_taint(&mut stack);
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
                reject_any_tainted_call_operand(old_index, callee_ref_tainted, &arg_taints)?;
                function.instruction(&remapper.instruction(op)?);
                for _ in 0..callee_sig.results {
                    stack.push(AnalysisValue::untainted());
                }
            }
            Operator::ReturnCallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                let callee_ref_tainted = pop_analysis_taint(&mut stack);
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
                reject_any_tainted_call_operand(old_index, callee_ref_tainted, &arg_taints)?;
                function.instruction(&remapper.instruction(op)?);
                stack.clear();
            }
            Operator::LocalGet { local_index } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(AnalysisValue::local(
                    local_index,
                    local_taints
                        .get(local_index as usize)
                        .copied()
                        .unwrap_or(false),
                ));
            }
            Operator::LocalSet { local_index } => {
                let value = pop_analysis_value(&mut stack);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value.tainted;
                }
                function.instruction(&remapper.instruction(op)?);
            }
            Operator::LocalTee { local_index } => {
                let value = stack.last().copied().unwrap_or_default();
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value.tainted;
                }
                function.instruction(&remapper.instruction(op)?);
            }
            Operator::I32Const { value } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(AnalysisValue::i32_const(value));
            }
            Operator::I64Const { .. } | Operator::F32Const { .. } | Operator::F64Const { .. } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(AnalysisValue::untainted());
            }
            Operator::Drop => {
                pop_analysis_taint(&mut stack);
                function.instruction(&remapper.instruction(op)?);
            }
            Operator::I32Load { memarg } => {
                let address = pop_analysis_value(&mut stack);
                let spilled_taint = if address.tainted {
                    false
                } else {
                    address
                        .offset_slot(memarg.offset)
                        .and_then(|slot| spilled_taints.get(&slot).copied())
                        .unwrap_or(false)
                };
                rewrite_load(
                    &mut function,
                    remapper,
                    address.tainted,
                    memarg,
                    report,
                    ScalarLoad::I32,
                )?;
                stack.push(AnalysisValue::with_taint(
                    load_result_taint(ScalarLoad::I32, address.tainted) || spilled_taint,
                ));
            }
            Operator::I32Load8S { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I32Load8S,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load8S,
                    tainted_address,
                )));
            }
            Operator::I32Load8U { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I32Load8U,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load8U,
                    tainted_address,
                )));
            }
            Operator::I32Load16S { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I32Load16S,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load16S,
                    tainted_address,
                )));
            }
            Operator::I32Load16U { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I32Load16U,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load16U,
                    tainted_address,
                )));
            }
            Operator::I64Load { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64,
                    tainted_address,
                )));
            }
            Operator::I64Load8S { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load8S,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load8S,
                    tainted_address,
                )));
            }
            Operator::I64Load8U { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load8U,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load8U,
                    tainted_address,
                )));
            }
            Operator::I64Load16S { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load16S,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load16S,
                    tainted_address,
                )));
            }
            Operator::I64Load16U { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load16U,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load16U,
                    tainted_address,
                )));
            }
            Operator::I64Load32S { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load32S,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load32S,
                    tainted_address,
                )));
            }
            Operator::I64Load32U { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::I64Load32U,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load32U,
                    tainted_address,
                )));
            }
            Operator::F32Load { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::F32,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::F32,
                    tainted_address,
                )));
            }
            Operator::F64Load { memarg } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                rewrite_load(
                    &mut function,
                    remapper,
                    tainted_address,
                    memarg,
                    report,
                    ScalarLoad::F64,
                )?;
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::F64,
                    tainted_address,
                )));
            }
            Operator::I32Store { memarg } => {
                let value = pop_analysis_value(&mut stack);
                let address = pop_analysis_value(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    address.tainted,
                    memarg,
                    report,
                    ScalarStore::I32,
                )?;
                if !address.tainted {
                    if let Some(slot) = address.offset_slot(memarg.offset) {
                        spilled_taints.insert(slot, value.tainted);
                    }
                }
            }
            Operator::I32Store8 { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I32Store8,
                )?;
            }
            Operator::I32Store16 { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I32Store16,
                )?;
            }
            Operator::I64Store { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I64,
                )?;
            }
            Operator::I64Store8 { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I64Store8,
                )?;
            }
            Operator::I64Store16 { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I64Store16,
                )?;
            }
            Operator::I64Store32 { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::I64Store32,
                )?;
            }
            Operator::F32Store { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::F32,
                )?;
            }
            Operator::F64Store { memarg } => {
                pop_analysis_taint(&mut stack);
                rewrite_store(
                    &mut function,
                    remapper,
                    pop_analysis_taint(&mut stack),
                    memarg,
                    report,
                    ScalarStore::F64,
                )?;
            }
            Operator::MemoryCopy { dst_mem, src_mem } => {
                let len_tainted = pop_analysis_taint(&mut stack);
                let src_tainted = pop_analysis_taint(&mut stack);
                let dst_tainted = pop_analysis_taint(&mut stack);
                rewrite_memory_copy(
                    &mut function,
                    remapper,
                    memory_copy_scratch.context("missing memory.copy scratch locals")?,
                    report,
                    dst_mem,
                    src_mem,
                    dst_tainted,
                    src_tainted,
                )?;
                let _ = len_tainted;
            }
            op if apply_analysis_value_taint_stack_effect(&op, &mut stack) => {
                function.instruction(&remapper.instruction(op)?);
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
    I32Load8S,
    I32Load8U,
    I32Load16S,
    I32Load16U,
    I64,
    I64Load8S,
    I64Load8U,
    I64Load16S,
    I64Load16U,
    I64Load32S,
    I64Load32U,
    F32,
    F64,
}

#[derive(Clone, Copy)]
enum ScalarStore {
    I32,
    I32Store8,
    I32Store16,
    I64,
    I64Store8,
    I64Store16,
    I64Store32,
    F32,
    F64,
}

#[derive(Clone, Copy)]
struct MemoryCopyScratch {
    dst: u32,
    src: u32,
    len: u32,
}

fn load_result_taint(kind: ScalarLoad, tainted_address: bool) -> bool {
    tainted_address && matches!(kind, ScalarLoad::I32)
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
            ScalarLoad::I32Load8S => {
                function.instruction(&Instruction::I32TLoad8S { memarg });
                report.i32_tloads += 1;
            }
            ScalarLoad::I32Load8U => {
                function.instruction(&Instruction::I32TLoad8U { memarg });
                report.i32_tloads += 1;
            }
            ScalarLoad::I32Load16S => {
                function.instruction(&Instruction::I32TLoad16S { memarg });
                report.i32_tloads += 1;
            }
            ScalarLoad::I32Load16U => {
                function.instruction(&Instruction::I32TLoad16U { memarg });
                report.i32_tloads += 1;
            }
            ScalarLoad::I64 => {
                function.instruction(&Instruction::I64TLoad { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load8S => {
                function.instruction(&Instruction::I64TLoad8S { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load8U => {
                function.instruction(&Instruction::I64TLoad8U { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load16S => {
                function.instruction(&Instruction::I64TLoad16S { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load16U => {
                function.instruction(&Instruction::I64TLoad16U { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load32S => {
                function.instruction(&Instruction::I64TLoad32S { memarg });
                report.i64_tloads += 1;
            }
            ScalarLoad::I64Load32U => {
                function.instruction(&Instruction::I64TLoad32U { memarg });
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
            ScalarLoad::I32Load8S => function.instruction(&Instruction::I32Load8S(memarg)),
            ScalarLoad::I32Load8U => function.instruction(&Instruction::I32Load8U(memarg)),
            ScalarLoad::I32Load16S => function.instruction(&Instruction::I32Load16S(memarg)),
            ScalarLoad::I32Load16U => function.instruction(&Instruction::I32Load16U(memarg)),
            ScalarLoad::I64 => function.instruction(&Instruction::I64Load(memarg)),
            ScalarLoad::I64Load8S => function.instruction(&Instruction::I64Load8S(memarg)),
            ScalarLoad::I64Load8U => function.instruction(&Instruction::I64Load8U(memarg)),
            ScalarLoad::I64Load16S => function.instruction(&Instruction::I64Load16S(memarg)),
            ScalarLoad::I64Load16U => function.instruction(&Instruction::I64Load16U(memarg)),
            ScalarLoad::I64Load32S => function.instruction(&Instruction::I64Load32S(memarg)),
            ScalarLoad::I64Load32U => function.instruction(&Instruction::I64Load32U(memarg)),
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
            ScalarStore::I32Store8 => {
                function.instruction(&Instruction::I32TStore8 { memarg });
                report.i32_tstores += 1;
            }
            ScalarStore::I32Store16 => {
                function.instruction(&Instruction::I32TStore16 { memarg });
                report.i32_tstores += 1;
            }
            ScalarStore::I64 => {
                function.instruction(&Instruction::I64TStore { memarg });
                report.i64_tstores += 1;
            }
            ScalarStore::I64Store8 => {
                function.instruction(&Instruction::I64TStore8 { memarg });
                report.i64_tstores += 1;
            }
            ScalarStore::I64Store16 => {
                function.instruction(&Instruction::I64TStore16 { memarg });
                report.i64_tstores += 1;
            }
            ScalarStore::I64Store32 => {
                function.instruction(&Instruction::I64TStore32 { memarg });
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
            ScalarStore::I32Store8 => function.instruction(&Instruction::I32Store8(memarg)),
            ScalarStore::I32Store16 => function.instruction(&Instruction::I32Store16(memarg)),
            ScalarStore::I64 => function.instruction(&Instruction::I64Store(memarg)),
            ScalarStore::I64Store8 => function.instruction(&Instruction::I64Store8(memarg)),
            ScalarStore::I64Store16 => function.instruction(&Instruction::I64Store16(memarg)),
            ScalarStore::I64Store32 => function.instruction(&Instruction::I64Store32(memarg)),
            ScalarStore::F32 => function.instruction(&Instruction::F32Store(memarg)),
            ScalarStore::F64 => function.instruction(&Instruction::F64Store(memarg)),
        };
    }
    Ok(())
}

fn rewrite_memory_copy(
    function: &mut Function,
    remapper: &mut IndexRemapper,
    scratch: MemoryCopyScratch,
    report: &mut RewriteReport,
    dst_mem: u32,
    src_mem: u32,
    dst_tainted: bool,
    src_tainted: bool,
) -> Result<()> {
    let dst_mem = remapper.memory_index(dst_mem)?;
    let src_mem = remapper.memory_index(src_mem)?;
    match (dst_tainted, src_tainted) {
        (false, false) => {
            function.instruction(&Instruction::MemoryCopy { src_mem, dst_mem });
        }
        (true, true) => {
            function.instruction(&Instruction::TMemoryCopy { src_mem, dst_mem });
        }
        (true, false) => {
            rewrite_mixed_memory_copy_loop(function, scratch, true, dst_mem, src_mem);
            report.i32_tstores += 1;
        }
        (false, true) => {
            rewrite_mixed_memory_copy_loop(function, scratch, false, dst_mem, src_mem);
            report.i32_tloads += 1;
        }
    }
    Ok(())
}

fn rewrite_mixed_memory_copy_loop(
    function: &mut Function,
    scratch: MemoryCopyScratch,
    dst_is_tmemory: bool,
    dst_mem: u32,
    src_mem: u32,
) {
    function.instruction(&Instruction::LocalSet(scratch.len));
    function.instruction(&Instruction::LocalSet(scratch.src));
    function.instruction(&Instruction::LocalSet(scratch.dst));

    emit_memory_copy_bounds_check(function, scratch.dst, scratch.len, dst_mem, dst_is_tmemory);
    emit_memory_copy_bounds_check(function, scratch.src, scratch.len, src_mem, !dst_is_tmemory);

    function.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    function.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    function.instruction(&Instruction::LocalGet(scratch.len));
    function.instruction(&Instruction::I32Eqz);
    function.instruction(&Instruction::BrIf(1));

    function.instruction(&Instruction::LocalGet(scratch.dst));
    function.instruction(&Instruction::LocalGet(scratch.src));
    if dst_is_tmemory {
        function.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: src_mem,
        }));
        function.instruction(&Instruction::I32TStore8 {
            memarg: wasm_encoder::MemArg {
                offset: 0,
                align: 0,
                memory_index: dst_mem,
            },
        });
    } else {
        function.instruction(&Instruction::I32TLoad8U {
            memarg: wasm_encoder::MemArg {
                offset: 0,
                align: 0,
                memory_index: src_mem,
            },
        });
        function.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: dst_mem,
        }));
    }

    function.instruction(&Instruction::LocalGet(scratch.dst));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalSet(scratch.dst));
    function.instruction(&Instruction::LocalGet(scratch.src));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalSet(scratch.src));
    function.instruction(&Instruction::LocalGet(scratch.len));
    function.instruction(&Instruction::I32Const(1));
    function.instruction(&Instruction::I32Sub);
    function.instruction(&Instruction::LocalSet(scratch.len));
    function.instruction(&Instruction::Br(0));
    function.instruction(&Instruction::End);
    function.instruction(&Instruction::End);
}

fn emit_memory_copy_bounds_check(
    function: &mut Function,
    ptr_local: u32,
    len_local: u32,
    memory_index: u32,
    is_tmemory: bool,
) {
    function.instruction(&Instruction::LocalGet(ptr_local));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::LocalGet(len_local));
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Add);
    if is_tmemory {
        function.instruction(&Instruction::TMemorySize { mem: memory_index });
    } else {
        function.instruction(&Instruction::MemorySize(memory_index));
    }
    function.instruction(&Instruction::I64ExtendI32U);
    function.instruction(&Instruction::I64Const(65_536));
    function.instruction(&Instruction::I64Mul);
    function.instruction(&Instruction::I64GtU);
    function.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    function.instruction(&Instruction::Unreachable);
    function.instruction(&Instruction::End);
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
        if let Some(marker) = mark_persistent_arg_marker(&operators, i, layout) {
            let param_index = marker.param_index();
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            markers.persistent_params.insert(param_index);
            i += match marker {
                PersistentArgMarker::ImmediateConst(_) => 2,
                PersistentArgMarker::SpilledConst(_) => 1,
            };
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

    for _ in 0..=function_count.saturating_mul(2) {
        let reader = BinaryReader::new(body_bytes, body_offset);
        let section = CodeSectionReader::new(reader)?;
        let mut changed = false;

        for (defined_index, body) in section.into_iter().enumerate() {
            let old_index = layout.imported_function_count + defined_index as u32;
            let analysis = analyze_function_taints_for_body(layout, old_index, &body?)?;
            let entry = layout
                .function_return_taints
                .get_mut(&old_index)
                .context("missing initialized return-taint entry")?;
            if *entry != analysis.return_taints {
                *entry = analysis.return_taints;
                changed = true;
            }
            let param_store_entry = layout
                .function_param_store_taints
                .entry(old_index)
                .or_default();
            if *param_store_entry != analysis.param_store_taints {
                *param_store_entry = analysis.param_store_taints;
                changed = true;
            }
            for (callee_index, params) in analysis.inferred_persistent_params {
                if callee_index < layout.imported_function_count {
                    continue;
                }
                let entry = layout
                    .persistent_param_markers
                    .entry(callee_index)
                    .or_default();
                for param in params {
                    if entry.insert(param) {
                        layout.transaction_func_markers.insert(callee_index);
                        changed = true;
                    }
                }
            }
        }

        if !changed {
            return Ok(());
        }
    }

    bail!("function return taint analysis did not converge")
}

#[derive(Default)]
struct BodyTaintAnalysis {
    return_taints: Vec<bool>,
    param_store_taints: BTreeMap<ParamStoreSlot, bool>,
    inferred_persistent_params: BTreeMap<u32, BTreeSet<u32>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct AnalysisStackSlot {
    base_local: u32,
    offset: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct ParamStoreSlot {
    param_index: u32,
    offset: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct AnalysisValue {
    tainted: bool,
    stack_slot: Option<AnalysisStackSlot>,
    const_i32: Option<i32>,
}

impl AnalysisValue {
    fn untainted() -> Self {
        Self::default()
    }

    fn tainted() -> Self {
        Self {
            tainted: true,
            ..Self::default()
        }
    }

    fn local(local_index: u32, tainted: bool) -> Self {
        Self {
            tainted,
            stack_slot: Some(AnalysisStackSlot {
                base_local: local_index,
                offset: 0,
            }),
            const_i32: None,
        }
    }

    fn i32_const(value: i32) -> Self {
        Self {
            const_i32: Some(value),
            ..Self::default()
        }
    }

    fn with_taint(tainted: bool) -> Self {
        Self {
            tainted,
            ..Self::default()
        }
    }

    fn offset_slot(self, offset: u64) -> Option<AnalysisStackSlot> {
        let mut slot = self.stack_slot?;
        slot.offset = slot.offset.checked_add(offset)?;
        Some(slot)
    }
}

fn analyze_function_taints_for_body(
    layout: &ModuleLayout,
    old_index: u32,
    body: &wasmparser::FunctionBody<'_>,
) -> Result<BodyTaintAnalysis> {
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
    if let Some(params) = layout.persistent_param_markers.get(&old_index) {
        for param in params {
            if (*param as usize) < sig.params {
                local_taints[*param as usize] = true;
            }
        }
    }
    let mut stack = Vec::<AnalysisValue>::new();
    let mut spilled_taints = BTreeMap::<AnalysisStackSlot, bool>::new();
    let mut branch_exit_taints = BTreeMap::<usize, BTreeMap<AnalysisStackSlot, bool>>::new();
    let mut return_taints = vec![false; sig.results];
    let mut param_store_taints = BTreeMap::<ParamStoreSlot, bool>::new();
    let mut inferred_persistent_params = BTreeMap::<u32, BTreeSet<u32>>::new();
    let mut block_depth = 0usize;

    let mut i = 0usize;
    while i < operators.len() {
        if let Some(marker) = mark_persistent_arg_marker(&operators, i, layout) {
            let param_index = marker.param_index();
            if param_index as usize >= sig.params {
                bail!(
                    "persistent arg marker refers to non-parameter {param_index} in function index {old_index}"
                );
            }
            local_taints[param_index as usize] = true;
            match marker {
                PersistentArgMarker::ImmediateConst(_) => {
                    i += 2;
                }
                PersistentArgMarker::SpilledConst(_) => {
                    pop_analysis_taint(&mut stack);
                    i += 1;
                }
            }
            continue;
        }

        match operators[i].clone() {
            Operator::Call { function_index } => {
                match layout.intrinsic_imports.get(&function_index) {
                    Some(IntrinsicKind::MarkTransactionFunc) => {}
                    Some(IntrinsicKind::PersistentAddrMut) => {
                        pop_analysis_taint(&mut stack);
                        stack.push(AnalysisValue::tainted());
                    }
                    Some(IntrinsicKind::MarkPersistentArg) => {
                        bail!(
                            "expected i32.const before __twasm_mark_persistent_arg in function index {old_index}"
                        );
                    }
                    None => {
                        let callee_sig = function_sig(layout, function_index)?;
                        let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                        let arg_taints = analysis_arg_taints(&args);
                        infer_direct_persistent_params(
                            layout,
                            &mut inferred_persistent_params,
                            function_index,
                            &arg_taints,
                        );
                        apply_callee_param_store_taints(
                            layout,
                            function_index,
                            &args,
                            &mut spilled_taints,
                        );
                        push_analysis_taints(
                            &mut stack,
                            function_return_taints(layout, function_index, callee_sig.results),
                        );
                    }
                }
            }
            Operator::ReturnCall { function_index } => {
                let callee_sig = function_sig(layout, function_index)?;
                let args = pop_analysis_call_args(&mut stack, callee_sig.params);
                let arg_taints = analysis_arg_taints(&args);
                infer_direct_persistent_params(
                    layout,
                    &mut inferred_persistent_params,
                    function_index,
                    &arg_taints,
                );
                merge_return_taints(
                    &mut return_taints,
                    &function_return_taints(layout, function_index, callee_sig.results),
                );
                stack.clear();
            }
            Operator::CallIndirect { type_index, .. } | Operator::CallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                pop_analysis_taint(&mut stack);
                for _ in 0..callee_sig.params {
                    pop_analysis_taint(&mut stack);
                }
                push_analysis_taints(&mut stack, vec![false; callee_sig.results]);
            }
            Operator::ReturnCallIndirect { type_index, .. }
            | Operator::ReturnCallRef { type_index } => {
                let callee_sig = type_sig(layout, type_index)?;
                pop_analysis_taint(&mut stack);
                for _ in 0..callee_sig.params {
                    pop_analysis_taint(&mut stack);
                }
                merge_return_taints(&mut return_taints, &vec![false; callee_sig.results]);
                stack.clear();
            }
            Operator::Block { .. } | Operator::Loop { .. } | Operator::If { .. } => {
                if matches!(operators[i], Operator::If { .. }) {
                    pop_analysis_taint(&mut stack);
                }
                block_depth += 1;
            }
            Operator::Else => {
                stack.clear();
            }
            Operator::Br { relative_depth } => {
                let target_depth = branch_target_depth_after_end(block_depth, relative_depth);
                merge_spilled_taints_into_branch_exit(
                    &mut branch_exit_taints,
                    target_depth,
                    &spilled_taints,
                );
                stack.clear();
            }
            Operator::BrIf { relative_depth } => {
                pop_analysis_taint(&mut stack);
                let target_depth = branch_target_depth_after_end(block_depth, relative_depth);
                merge_spilled_taints_into_branch_exit(
                    &mut branch_exit_taints,
                    target_depth,
                    &spilled_taints,
                );
            }
            Operator::LocalGet { local_index } => {
                stack.push(AnalysisValue::local(
                    local_index,
                    local_taints
                        .get(local_index as usize)
                        .copied()
                        .unwrap_or(false),
                ));
            }
            Operator::LocalSet { local_index } => {
                let value = pop_analysis_value(&mut stack);
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value.tainted;
                }
            }
            Operator::LocalTee { local_index } => {
                let value = stack.last().copied().unwrap_or_default();
                if let Some(slot) = local_taints.get_mut(local_index as usize) {
                    *slot = value.tainted;
                }
            }
            Operator::I32Const { value } => stack.push(AnalysisValue::i32_const(value)),
            Operator::I64Const { .. } | Operator::F32Const { .. } | Operator::F64Const { .. } => {
                stack.push(AnalysisValue::untainted())
            }
            Operator::I32Load { memarg } => {
                let address = pop_analysis_value(&mut stack);
                let spilled_taint = if address.tainted {
                    false
                } else {
                    address
                        .offset_slot(memarg.offset)
                        .and_then(|slot| spilled_taints.get(&slot).copied())
                        .unwrap_or(false)
                };
                stack.push(AnalysisValue::with_taint(
                    load_result_taint(ScalarLoad::I32, address.tainted) || spilled_taint,
                ));
            }
            Operator::I32Load8S { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load8S,
                    tainted_address,
                )));
            }
            Operator::I32Load8U { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load8U,
                    tainted_address,
                )));
            }
            Operator::I32Load16S { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load16S,
                    tainted_address,
                )));
            }
            Operator::I32Load16U { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I32Load16U,
                    tainted_address,
                )));
            }
            Operator::I64Load { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64,
                    tainted_address,
                )));
            }
            Operator::I64Load8S { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load8S,
                    tainted_address,
                )));
            }
            Operator::I64Load8U { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load8U,
                    tainted_address,
                )));
            }
            Operator::I64Load16S { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load16S,
                    tainted_address,
                )));
            }
            Operator::I64Load16U { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load16U,
                    tainted_address,
                )));
            }
            Operator::I64Load32S { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load32S,
                    tainted_address,
                )));
            }
            Operator::I64Load32U { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::I64Load32U,
                    tainted_address,
                )));
            }
            Operator::F32Load { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::F32,
                    tainted_address,
                )));
            }
            Operator::F64Load { .. } => {
                let tainted_address = pop_analysis_taint(&mut stack);
                stack.push(AnalysisValue::with_taint(load_result_taint(
                    ScalarLoad::F64,
                    tainted_address,
                )));
            }
            Operator::I32Store { memarg } => {
                let value = pop_analysis_value(&mut stack);
                let address = pop_analysis_value(&mut stack);
                if !address.tainted {
                    if let Some(slot) = address.offset_slot(memarg.offset) {
                        spilled_taints.insert(slot, value.tainted);
                        if slot.base_local < sig.params as u32 {
                            param_store_taints.insert(
                                ParamStoreSlot {
                                    param_index: slot.base_local,
                                    offset: slot.offset,
                                },
                                value.tainted,
                            );
                        }
                    }
                }
            }
            Operator::I32Store8 { .. }
            | Operator::I32Store16 { .. }
            | Operator::I64Store { .. }
            | Operator::I64Store8 { .. }
            | Operator::I64Store16 { .. }
            | Operator::I64Store32 { .. }
            | Operator::F32Store { .. }
            | Operator::F64Store { .. } => {
                pop_analysis_taint(&mut stack);
                pop_analysis_taint(&mut stack);
            }
            Operator::MemoryCopy { .. } => {
                pop_analysis_taint(&mut stack);
                pop_analysis_taint(&mut stack);
                pop_analysis_taint(&mut stack);
            }
            Operator::Drop => {
                pop_analysis_taint(&mut stack);
            }
            op if apply_analysis_value_taint_stack_effect(&op, &mut stack) => {}
            Operator::Return => {
                merge_return_taints(
                    &mut return_taints,
                    &analysis_stack_results(&stack, sig.results),
                );
                stack.clear();
            }
            Operator::End => {
                if block_depth == 0 {
                    merge_return_taints(
                        &mut return_taints,
                        &analysis_stack_results(&stack, sig.results),
                    );
                    stack.clear();
                } else {
                    block_depth -= 1;
                    merge_branch_exit_taints(
                        &mut spilled_taints,
                        &mut branch_exit_taints,
                        block_depth,
                    );
                    stack.clear();
                }
            }
            _ => {
                stack.clear();
            }
        }

        i += 1;
    }

    Ok(BodyTaintAnalysis {
        return_taints,
        param_store_taints,
        inferred_persistent_params,
    })
}

fn infer_direct_persistent_params(
    layout: &ModuleLayout,
    inferred: &mut BTreeMap<u32, BTreeSet<u32>>,
    callee_index: u32,
    arg_taints: &[bool],
) {
    if callee_index < layout.imported_function_count
        || layout.intrinsic_imports.contains_key(&callee_index)
    {
        return;
    }

    for (param_index, tainted) in arg_taints.iter().enumerate() {
        if *tainted {
            inferred
                .entry(callee_index)
                .or_default()
                .insert(param_index as u32);
        }
    }
}

fn branch_target_depth_after_end(block_depth: usize, relative_depth: u32) -> usize {
    block_depth.saturating_sub(relative_depth as usize + 1)
}

fn merge_spilled_taints_into_branch_exit(
    branch_exit_taints: &mut BTreeMap<usize, BTreeMap<AnalysisStackSlot, bool>>,
    target_depth: usize,
    spilled_taints: &BTreeMap<AnalysisStackSlot, bool>,
) {
    let exits = branch_exit_taints.entry(target_depth).or_default();
    for (slot, tainted) in spilled_taints {
        let entry = exits.entry(*slot).or_insert(false);
        *entry |= *tainted;
    }
}

fn merge_branch_exit_taints(
    spilled_taints: &mut BTreeMap<AnalysisStackSlot, bool>,
    branch_exit_taints: &mut BTreeMap<usize, BTreeMap<AnalysisStackSlot, bool>>,
    target_depth: usize,
) {
    let Some(exits) = branch_exit_taints.remove(&target_depth) else {
        return;
    };
    for (slot, tainted) in exits {
        let entry = spilled_taints.entry(slot).or_insert(false);
        *entry |= tainted;
    }
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

fn mark_persistent_arg_marker(
    operators: &[Operator<'_>],
    index: usize,
    layout: &ModuleLayout,
) -> Option<PersistentArgMarker> {
    if let Operator::I32Const { value } = operators.get(index)? {
        let Operator::Call { function_index } = operators.get(index + 1)? else {
            return None;
        };
        if layout.intrinsic_imports.get(function_index) != Some(&IntrinsicKind::MarkPersistentArg) {
            return None;
        }
        return u32::try_from(*value)
            .ok()
            .map(PersistentArgMarker::ImmediateConst);
    }

    let Operator::Call { function_index } = operators.get(index)? else {
        return None;
    };
    if layout.intrinsic_imports.get(function_index) != Some(&IntrinsicKind::MarkPersistentArg) {
        return None;
    }
    let Some(Operator::LocalGet { local_index }) =
        index.checked_sub(1).and_then(|i| operators.get(i))
    else {
        return None;
    };
    const_local_value_before(operators, index - 1, *local_index)
        .map(PersistentArgMarker::SpilledConst)
}

fn const_local_value_before(
    operators: &[Operator<'_>],
    before_index: usize,
    local_index: u32,
) -> Option<u32> {
    for i in (0..before_index).rev() {
        match operators.get(i)? {
            Operator::LocalSet {
                local_index: assigned,
            }
            | Operator::LocalTee {
                local_index: assigned,
            } if *assigned == local_index => {
                let Operator::I32Const { value } = operators.get(i.checked_sub(1)?)? else {
                    return None;
                };
                return u32::try_from(*value).ok();
            }
            _ => {}
        }
    }

    None
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

fn pop_analysis_value(stack: &mut Vec<AnalysisValue>) -> AnalysisValue {
    stack.pop().unwrap_or_default()
}

fn pop_analysis_taint(stack: &mut Vec<AnalysisValue>) -> bool {
    pop_analysis_value(stack).tainted
}

fn pop_analysis_call_args(stack: &mut Vec<AnalysisValue>, params: usize) -> Vec<AnalysisValue> {
    let mut args = vec![AnalysisValue::default(); params];
    for param_index in (0..params).rev() {
        args[param_index] = pop_analysis_value(stack);
    }
    args
}

fn analysis_arg_taints(args: &[AnalysisValue]) -> Vec<bool> {
    args.iter().map(|arg| arg.tainted).collect()
}

fn push_analysis_taints(stack: &mut Vec<AnalysisValue>, taints: Vec<bool>) {
    stack.extend(taints.into_iter().map(AnalysisValue::with_taint));
}

fn apply_callee_param_store_taints(
    layout: &ModuleLayout,
    callee_index: u32,
    args: &[AnalysisValue],
    spilled_taints: &mut BTreeMap<AnalysisStackSlot, bool>,
) {
    let Some(param_store_taints) = layout.function_param_store_taints.get(&callee_index) else {
        return;
    };

    for (param_slot, tainted) in param_store_taints {
        let Some(arg) = args.get(param_slot.param_index as usize) else {
            continue;
        };
        let Some(caller_slot) = arg.offset_slot(param_slot.offset) else {
            continue;
        };
        spilled_taints.insert(caller_slot, *tainted);
    }
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

fn analysis_stack_results(stack: &[AnalysisValue], results: usize) -> Vec<bool> {
    if results == 0 {
        return Vec::new();
    }
    let start = stack.len().saturating_sub(results);
    let mut taints = stack[start..]
        .iter()
        .map(|value| value.tainted)
        .collect::<Vec<_>>();
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

fn apply_analysis_value_taint_stack_effect(
    op: &Operator<'_>,
    stack: &mut Vec<AnalysisValue>,
) -> bool {
    match op {
        Operator::I32Add => {
            let rhs = pop_analysis_value(stack);
            let lhs = pop_analysis_value(stack);
            stack.push(analysis_i32_add(lhs, rhs));
            true
        }
        op if is_taint_preserving_unary_op(op) => {
            let value = pop_analysis_value(stack);
            stack.push(AnalysisValue::with_taint(value.tainted));
            true
        }
        op if is_taint_dropping_unary_result_op(op) => {
            pop_analysis_taint(stack);
            stack.push(AnalysisValue::untainted());
            true
        }
        op if is_taint_merging_binary_op(op) => {
            let rhs = pop_analysis_taint(stack);
            let lhs = pop_analysis_taint(stack);
            stack.push(AnalysisValue::with_taint(lhs || rhs));
            true
        }
        op if is_taint_dropping_binary_result_op(op) => {
            pop_analysis_taint(stack);
            pop_analysis_taint(stack);
            stack.push(AnalysisValue::untainted());
            true
        }
        Operator::Select | Operator::TypedSelect { .. } => {
            pop_analysis_taint(stack);
            let rhs = pop_analysis_taint(stack);
            let lhs = pop_analysis_taint(stack);
            stack.push(AnalysisValue::with_taint(lhs || rhs));
            true
        }
        Operator::MemorySize { .. } | Operator::TableSize { .. } | Operator::TTableSize { .. } => {
            stack.push(AnalysisValue::untainted());
            true
        }
        Operator::MemoryGrow { .. }
        | Operator::TableGet { .. }
        | Operator::TTableGet { .. }
        | Operator::TMemoryGrow { .. } => {
            pop_analysis_taint(stack);
            stack.push(AnalysisValue::untainted());
            true
        }
        Operator::TableGrow { .. } | Operator::TTableGrow { .. } => {
            pop_analysis_taint(stack);
            pop_analysis_taint(stack);
            stack.push(AnalysisValue::untainted());
            true
        }
        _ => false,
    }
}

fn analysis_i32_add(lhs: AnalysisValue, rhs: AnalysisValue) -> AnalysisValue {
    let tainted = lhs.tainted || rhs.tainted;
    let const_i32 = lhs
        .const_i32
        .and_then(|lhs| rhs.const_i32.and_then(|rhs| lhs.checked_add(rhs)));
    let stack_slot = if tainted {
        None
    } else {
        match (lhs.stack_slot, lhs.const_i32, rhs.stack_slot, rhs.const_i32) {
            (Some(slot), _, _, Some(offset)) => offset_analysis_stack_slot(slot, offset),
            (_, Some(offset), Some(slot), _) => offset_analysis_stack_slot(slot, offset),
            _ => None,
        }
    };

    AnalysisValue {
        tainted,
        stack_slot,
        const_i32,
    }
}

fn offset_analysis_stack_slot(
    mut slot: AnalysisStackSlot,
    offset: i32,
) -> Option<AnalysisStackSlot> {
    if offset >= 0 {
        slot.offset = slot.offset.checked_add(offset as u64)?;
    } else {
        slot.offset = slot.offset.checked_sub(offset.unsigned_abs() as u64)?;
    }
    Some(slot)
}

fn is_taint_preserving_unary_op(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::I32Clz
            | Operator::I32Ctz
            | Operator::I32Popcnt
            | Operator::I64Clz
            | Operator::I64Ctz
            | Operator::I64Popcnt
            | Operator::F32Abs
            | Operator::F32Neg
            | Operator::F32Ceil
            | Operator::F32Floor
            | Operator::F32Trunc
            | Operator::F32Nearest
            | Operator::F32Sqrt
            | Operator::F64Abs
            | Operator::F64Neg
            | Operator::F64Ceil
            | Operator::F64Floor
            | Operator::F64Trunc
            | Operator::F64Nearest
            | Operator::F64Sqrt
            | Operator::I32WrapI64
            | Operator::I32TruncF32S
            | Operator::I32TruncF32U
            | Operator::I32TruncF64S
            | Operator::I32TruncF64U
            | Operator::I64ExtendI32S
            | Operator::I64ExtendI32U
            | Operator::I64TruncF32S
            | Operator::I64TruncF32U
            | Operator::I64TruncF64S
            | Operator::I64TruncF64U
            | Operator::F32ConvertI32S
            | Operator::F32ConvertI32U
            | Operator::F32ConvertI64S
            | Operator::F32ConvertI64U
            | Operator::F32DemoteF64
            | Operator::F64ConvertI32S
            | Operator::F64ConvertI32U
            | Operator::F64ConvertI64S
            | Operator::F64ConvertI64U
            | Operator::F64PromoteF32
            | Operator::I32ReinterpretF32
            | Operator::I64ReinterpretF64
            | Operator::F32ReinterpretI32
            | Operator::F64ReinterpretI64
            | Operator::I32Extend8S
            | Operator::I32Extend16S
            | Operator::I64Extend8S
            | Operator::I64Extend16S
            | Operator::I64Extend32S
            | Operator::I32TruncSatF32S
            | Operator::I32TruncSatF32U
            | Operator::I32TruncSatF64S
            | Operator::I32TruncSatF64U
            | Operator::I64TruncSatF32S
            | Operator::I64TruncSatF32U
            | Operator::I64TruncSatF64S
            | Operator::I64TruncSatF64U
    )
}

fn is_taint_dropping_unary_result_op(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::I32Eqz | Operator::I64Eqz | Operator::RefIsNull
    )
}

fn is_taint_merging_binary_op(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::I32Add
            | Operator::I32Sub
            | Operator::I32Mul
            | Operator::I32DivS
            | Operator::I32DivU
            | Operator::I32RemS
            | Operator::I32RemU
            | Operator::I32And
            | Operator::I32Or
            | Operator::I32Xor
            | Operator::I32Shl
            | Operator::I32ShrS
            | Operator::I32ShrU
            | Operator::I32Rotl
            | Operator::I32Rotr
            | Operator::I64Add
            | Operator::I64Sub
            | Operator::I64Mul
            | Operator::I64DivS
            | Operator::I64DivU
            | Operator::I64RemS
            | Operator::I64RemU
            | Operator::I64And
            | Operator::I64Or
            | Operator::I64Xor
            | Operator::I64Shl
            | Operator::I64ShrS
            | Operator::I64ShrU
            | Operator::I64Rotl
            | Operator::I64Rotr
            | Operator::F32Add
            | Operator::F32Sub
            | Operator::F32Mul
            | Operator::F32Div
            | Operator::F32Min
            | Operator::F32Max
            | Operator::F32Copysign
            | Operator::F64Add
            | Operator::F64Sub
            | Operator::F64Mul
            | Operator::F64Div
            | Operator::F64Min
            | Operator::F64Max
            | Operator::F64Copysign
    )
}

fn is_taint_dropping_binary_result_op(op: &Operator<'_>) -> bool {
    matches!(
        op,
        Operator::I32Eq
            | Operator::I32Ne
            | Operator::I32LtS
            | Operator::I32LtU
            | Operator::I32GtS
            | Operator::I32GtU
            | Operator::I32LeS
            | Operator::I32LeU
            | Operator::I32GeS
            | Operator::I32GeU
            | Operator::I64Eq
            | Operator::I64Ne
            | Operator::I64LtS
            | Operator::I64LtU
            | Operator::I64GtS
            | Operator::I64GtU
            | Operator::I64LeS
            | Operator::I64LeU
            | Operator::I64GeS
            | Operator::I64GeU
            | Operator::F32Eq
            | Operator::F32Ne
            | Operator::F32Lt
            | Operator::F32Gt
            | Operator::F32Le
            | Operator::F32Ge
            | Operator::F64Eq
            | Operator::F64Ne
            | Operator::F64Lt
            | Operator::F64Gt
            | Operator::F64Le
            | Operator::F64Ge
            | Operator::RefEq
    )
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
        bail!(
            "persistent pointer escapes to unrecognized call in function index {caller_index} \
             calling {callee_index} with tainted args {arg_taints:?}"
        );
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

fn call_is_immediately_unreachable(operators: &[Operator<'_>], call_index: usize) -> bool {
    matches!(operators.get(call_index + 1), Some(Operator::Unreachable))
}

pub fn main() -> anyhow::Result<()> {
    crate::rust::cli::main()
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
