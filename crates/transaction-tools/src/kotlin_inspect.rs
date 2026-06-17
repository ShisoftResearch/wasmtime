use serde::Serialize;

use crate::kotlin_metadata::KotlinSidecar;

#[derive(Debug, Serialize)]
pub struct KotlinInspectReport {
    pub module: String,
    pub persistent_types: usize,
    pub transaction_functions: usize,
    pub roots: usize,
}

pub fn inspect_sidecar(sidecar: &KotlinSidecar) -> KotlinInspectReport {
    KotlinInspectReport {
        module: sidecar.module.clone(),
        persistent_types: sidecar.persistent_types.len(),
        transaction_functions: sidecar.transaction_functions.len(),
        roots: sidecar.roots.len(),
    }
}
