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
    func_index_map: Vec<Option<u32>>,
    func_sigs: Vec<FuncSig>,
    intrinsic_imports: BTreeMap<u32, IntrinsicKind>,
    imported_function_count: u32,
    has_memory_zero: bool,
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

    if layout.has_memory_zero {
        let metadata = encode_transaction_objects(&transaction_functions);
        module.section(&CustomSection {
            name: Cow::Borrowed(TRANSACTION_OBJECTS_CUSTOM_SECTION),
            data: Cow::Owned(metadata),
        });
    }

    Ok((module.finish(), report))
}

fn analyze_module(input: &[u8]) -> Result<ModuleLayout> {
    let mut type_sigs = Vec::new();
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
                    type_sigs.push(FuncSig {
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
                            let sig = *type_sigs
                                .get(type_index as usize)
                                .context("function import referenced missing type")?;
                            layout.func_sigs.push(sig);
                            let intrinsic = intrinsic_kind(module, name);
                            layout.func_index_map.push(intrinsic.map(|_| 0).or(Some(next_new_func)));
                            if let Some(intrinsic) = intrinsic {
                                layout.intrinsic_imports.insert(next_old_func, intrinsic);
                                *layout
                                    .func_index_map
                                    .last_mut()
                                    .expect("pushed function index entry") = None;
                            } else {
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
                    let sig = *type_sigs
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
            Payload::CodeSectionStart { count, .. } => {
                if saw_function_section && count != (next_old_func - layout.imported_function_count) {
                    bail!("function and code section length mismatch");
                }
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
    let mut local_taints = vec![false; sig.params + locals.iter().map(|(count, _)| *count as usize).sum::<usize>()];
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
            Operator::Call { function_index } => match layout.intrinsic_imports.get(&function_index) {
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
                    bail!("expected i32.const before __twasm_mark_persistent_arg in function index {old_index}");
                }
                None => {
                    let callee_sig = *layout
                        .func_sigs
                        .get(function_index as usize)
                        .context("missing callee signature")?;
                    let mut tainted_arg = false;
                    for _ in 0..callee_sig.params {
                        tainted_arg |= pop_taint(&mut stack);
                    }
                    if tainted_arg {
                        bail!("persistent pointer escapes to unrecognized call in function index {old_index}");
                    }
                    function.instruction(&Instruction::Call(
                        remapper.remap_function_index(function_index)?,
                    ));
                    for _ in 0..callee_sig.results {
                        stack.push(false);
                    }
                }
            },
            Operator::LocalGet { local_index } => {
                function.instruction(&remapper.instruction(op)?);
                stack.push(local_taints.get(local_index as usize).copied().unwrap_or(false));
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
            Operator::I32Add
            | Operator::I64Add
            | Operator::F32Add
            | Operator::F64Add => {
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

fn collect_locals(body: &wasmparser::FunctionBody<'_>) -> Result<Vec<(u32, wasm_encoder::ValType)>> {
    let mut locals = Vec::new();
    for local in body.get_locals_reader()? {
        let (count, ty) = local?;
        locals.push((count, RoundtripReencoder.val_type(ty)?));
    }
    Ok(locals)
}

fn intrinsic_kind(module: &str, name: &str) -> Option<IntrinsicKind> {
    if module != INTRINSIC_MODULE {
        return None;
    }
    match name {
        PERSISTENT_ADDR_MUT => Some(IntrinsicKind::PersistentAddrMut),
        MARK_TRANSACTION_FUNC => Some(IntrinsicKind::MarkTransactionFunc),
        MARK_PERSISTENT_ARG => Some(IntrinsicKind::MarkPersistentArg),
        _ => None,
    }
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
