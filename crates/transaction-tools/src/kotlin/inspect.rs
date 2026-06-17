use anyhow::{Context, Result};
use serde::Serialize;
use wasmparser::{CompositeInnerType, Operator, Parser, Payload};

use super::metadata::KotlinSidecar;

#[derive(Debug, Serialize)]
pub struct KotlinInspectReport {
    pub module: String,
    pub persistent_types: usize,
    pub transaction_functions: usize,
    pub roots: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct KotlinWasmShapeReport {
    pub functions: usize,
    pub gc_struct_types: usize,
    pub gc_array_types: usize,
    pub struct_new_ops: usize,
    pub array_new_ops: usize,
    pub struct_get_ops: usize,
    pub struct_set_ops: usize,
    pub array_get_ops: usize,
    pub array_set_ops: usize,
    pub array_len_ops: usize,
    pub marker_imports: usize,
}

pub fn inspect_sidecar(sidecar: &KotlinSidecar) -> KotlinInspectReport {
    KotlinInspectReport {
        module: sidecar.module.clone(),
        persistent_types: sidecar.persistent_types.len(),
        transaction_functions: sidecar.transaction_functions.len(),
        roots: sidecar.roots.len(),
    }
}

pub fn inspect_wasm_shape(bytes: &[u8]) -> Result<KotlinWasmShapeReport> {
    let mut report = KotlinWasmShapeReport::default();

    for payload in Parser::new(0).parse_all(bytes) {
        match payload.context("failed to parse Kotlin wasm payload")? {
            Payload::TypeSection(section) => {
                for group in section {
                    let group = group.context("failed to parse Kotlin wasm type group")?;
                    for ty in group.into_types() {
                        match ty.composite_type.inner {
                            CompositeInnerType::Struct(_) => report.gc_struct_types += 1,
                            CompositeInnerType::Array(_) => report.gc_array_types += 1,
                            _ => {}
                        }
                    }
                }
            }
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    let import = import.context("failed to parse Kotlin wasm import")?;
                    if import.module.starts_with("twasm") {
                        report.marker_imports += 1;
                    }
                }
            }
            Payload::FunctionSection(section) => {
                report.functions += section.count() as usize;
            }
            Payload::CodeSectionEntry(body) => {
                let mut reader = body
                    .get_operators_reader()
                    .context("failed to read Kotlin wasm operators")?;
                while !reader.eof() {
                    match reader
                        .read()
                        .context("failed to parse Kotlin wasm operator")?
                    {
                        Operator::StructNew { .. } | Operator::StructNewDefault { .. } => {
                            report.struct_new_ops += 1;
                        }
                        Operator::StructGet { .. }
                        | Operator::StructGetS { .. }
                        | Operator::StructGetU { .. } => {
                            report.struct_get_ops += 1;
                        }
                        Operator::StructSet { .. } => {
                            report.struct_set_ops += 1;
                        }
                        Operator::ArrayNew { .. }
                        | Operator::ArrayNewDefault { .. }
                        | Operator::ArrayNewFixed { .. }
                        | Operator::ArrayNewData { .. }
                        | Operator::ArrayNewElem { .. } => {
                            report.array_new_ops += 1;
                        }
                        Operator::ArrayGet { .. }
                        | Operator::ArrayGetS { .. }
                        | Operator::ArrayGetU { .. } => {
                            report.array_get_ops += 1;
                        }
                        Operator::ArraySet { .. } => {
                            report.array_set_ops += 1;
                        }
                        Operator::ArrayLen => {
                            report.array_len_ops += 1;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    Ok(report)
}
