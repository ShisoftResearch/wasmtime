use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Read;

const KOTLIN_SIDECAR_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinSidecar {
    pub version: u32,
    pub module: String,
    #[serde(default)]
    pub gc_wasm: KotlinGcWasmPolicy,
    pub persistent_types: Vec<KotlinPersistentType>,
    #[serde(default)]
    pub copyable_types: Vec<KotlinCopyableType>,
    pub transaction_functions: Vec<String>,
    pub roots: Vec<KotlinRoot>,
}

impl KotlinSidecar {
    pub fn new(
        module: impl Into<String>,
        persistent_types: Vec<KotlinPersistentType>,
        transaction_functions: Vec<String>,
        roots: Vec<KotlinRoot>,
    ) -> Self {
        Self {
            version: KOTLIN_SIDECAR_VERSION,
            module: module.into(),
            gc_wasm: KotlinGcWasmPolicy::default(),
            persistent_types,
            copyable_types: Vec::new(),
            transaction_functions,
            roots,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinGcWasmPolicy {
    #[serde(default)]
    pub capture: KotlinGcWasmCapture,
    #[serde(default)]
    pub deny_types: Vec<String>,
}

impl Default for KotlinGcWasmPolicy {
    fn default() -> Self {
        Self {
            capture: KotlinGcWasmCapture::SidecarTypes,
            deny_types: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinGcWasmCapture {
    SidecarTypes,
    AllModuleGcTypes,
}

impl Default for KotlinGcWasmCapture {
    fn default() -> Self {
        Self::SidecarTypes
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinPersistentType {
    pub name: String,
    pub kind: KotlinPersistentKind,
    #[serde(default)]
    pub fields: Vec<KotlinField>,
    #[serde(default)]
    pub element: Option<KotlinField>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinCopyableType {
    pub name: String,
    pub kind: KotlinPersistentKind,
    #[serde(default)]
    pub fields: Vec<KotlinField>,
    #[serde(default)]
    pub element: Option<KotlinField>,
    #[serde(default)]
    pub source: KotlinCopyableSource,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinCopyableSource {
    TxCopyable,
    Serializable,
    Builtin,
}

impl Default for KotlinCopyableSource {
    fn default() -> Self {
        Self::TxCopyable
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinPersistentKind {
    Struct,
    Array,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinField {
    pub name: String,
    pub kind: KotlinFieldKind,
    #[serde(default)]
    pub r#type: Option<String>,
    pub nullable: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum KotlinFieldKind {
    I31,
    I32,
    I64,
    F32,
    F64,
    V128,
    Ref,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct KotlinRoot {
    pub name: String,
    pub r#type: String,
    pub nullable: bool,
}

pub fn parse_kotlin_sidecar(mut bytes: impl Read) -> Result<KotlinSidecar> {
    let mut raw = Vec::new();
    bytes
        .read_to_end(&mut raw)
        .context("failed to read Kotlin metadata sidecar bytes")?;

    let sidecar: KotlinSidecar = serde_json::from_slice(&raw)
        .map_err(|err| anyhow!("failed to parse Kotlin metadata sidecar JSON: {err}"))?;
    validate_kotlin_sidecar(&sidecar)?;
    Ok(sidecar)
}

pub fn validate_kotlin_sidecar(sidecar: &KotlinSidecar) -> Result<()> {
    if sidecar.version != KOTLIN_SIDECAR_VERSION {
        bail!(
            "unsupported Kotlin metadata sidecar version: expected {}, found {}",
            KOTLIN_SIDECAR_VERSION,
            sidecar.version
        );
    }

    let mut persistent_type_names = HashSet::with_capacity(sidecar.persistent_types.len());
    let allow_unknown_gc_types = sidecar.gc_wasm.capture == KotlinGcWasmCapture::AllModuleGcTypes;
    for persistent_type in &sidecar.persistent_types {
        if persistent_type.name.is_empty() {
            bail!("persistent type name must not be empty");
        }

        if !persistent_type_names.insert(persistent_type.name.as_str()) {
            bail!("duplicate persistent type name: {}", persistent_type.name);
        }
    }

    let mut copyable_type_names = HashSet::with_capacity(sidecar.copyable_types.len());
    for copyable_type in &sidecar.copyable_types {
        if copyable_type.name.is_empty() {
            bail!("copyable type name must not be empty");
        }

        if persistent_type_names.contains(copyable_type.name.as_str()) {
            bail!(
                "copyable type {} is redundant: @Persistent already implies TxCopyable",
                copyable_type.name
            );
        }

        if !copyable_type_names.insert(copyable_type.name.as_str()) {
            bail!("duplicate copyable type name: {}", copyable_type.name);
        }
    }

    let mut transaction_function_names =
        HashSet::with_capacity(sidecar.transaction_functions.len());
    for name in &sidecar.transaction_functions {
        if name.is_empty() {
            bail!("transaction function name must not be empty");
        }
        if !transaction_function_names.insert(name.as_str()) {
            bail!("duplicate transaction function name: {name}");
        }
    }

    for persistent_type in &sidecar.persistent_types {
        match persistent_type.kind {
            KotlinPersistentKind::Struct => {
                if persistent_type.element.is_some() {
                    bail!(
                        "struct persistent type {} must not define an element",
                        persistent_type.name
                    );
                }

                let mut field_names = HashSet::with_capacity(persistent_type.fields.len());
                for field in &persistent_type.fields {
                    if field.name.is_empty() {
                        bail!(
                            "field name in persistent type {} must not be empty",
                            persistent_type.name
                        );
                    }
                    if !field_names.insert(field.name.as_str()) {
                        bail!(
                            "duplicate field name {} in persistent type {}",
                            field.name,
                            persistent_type.name
                        );
                    }
                    validate_field(
                        field,
                        &persistent_type_names,
                        allow_unknown_gc_types,
                        format_args!(
                            "persistent type {} field {}",
                            persistent_type.name, field.name
                        ),
                        "persistent type",
                    )?;
                }
            }
            KotlinPersistentKind::Array => {
                if !persistent_type.fields.is_empty() {
                    bail!(
                        "array persistent type {} must not define fields",
                        persistent_type.name
                    );
                }

                let Some(element) = persistent_type.element.as_ref() else {
                    bail!(
                        "array persistent type {} must define an element",
                        persistent_type.name
                    );
                };

                if element.name.is_empty() {
                    bail!(
                        "array element name in persistent type {} must not be empty",
                        persistent_type.name
                    );
                }
                validate_field(
                    element,
                    &persistent_type_names,
                    allow_unknown_gc_types,
                    format_args!("persistent type {} element", persistent_type.name),
                    "persistent type",
                )?;
            }
        }
    }

    let copyable_ref_targets: HashSet<&str> = persistent_type_names
        .iter()
        .copied()
        .chain(copyable_type_names.iter().copied())
        .collect();

    for copyable_type in &sidecar.copyable_types {
        match copyable_type.kind {
            KotlinPersistentKind::Struct => {
                if copyable_type.element.is_some() {
                    bail!(
                        "struct copyable type {} must not define an element",
                        copyable_type.name
                    );
                }

                let mut field_names = HashSet::with_capacity(copyable_type.fields.len());
                for field in &copyable_type.fields {
                    if field.name.is_empty() {
                        bail!(
                            "field name in copyable type {} must not be empty",
                            copyable_type.name
                        );
                    }
                    if !field_names.insert(field.name.as_str()) {
                        bail!(
                            "duplicate field name {} in copyable type {}",
                            field.name,
                            copyable_type.name
                        );
                    }
                    validate_field(
                        field,
                        &copyable_ref_targets,
                        allow_unknown_gc_types,
                        format_args!("copyable type {} field {}", copyable_type.name, field.name),
                        "persistent or copyable type",
                    )?;
                }
            }
            KotlinPersistentKind::Array => {
                if !copyable_type.fields.is_empty() {
                    bail!(
                        "array copyable type {} must not define fields",
                        copyable_type.name
                    );
                }

                let Some(element) = copyable_type.element.as_ref() else {
                    bail!(
                        "array copyable type {} must define an element",
                        copyable_type.name
                    );
                };

                if element.name.is_empty() {
                    bail!(
                        "array element name in copyable type {} must not be empty",
                        copyable_type.name
                    );
                }
                validate_field(
                    element,
                    &copyable_ref_targets,
                    allow_unknown_gc_types,
                    format_args!("copyable type {} element", copyable_type.name),
                    "persistent or copyable type",
                )?;
            }
        }
    }

    let mut root_names = HashSet::with_capacity(sidecar.roots.len());
    for root in &sidecar.roots {
        if root.name.is_empty() {
            bail!("root name must not be empty");
        }
        if !root_names.insert(root.name.as_str()) {
            bail!("duplicate root name: {}", root.name);
        }
        if !allow_unknown_gc_types && !persistent_type_names.contains(root.r#type.as_str()) {
            bail!("unknown root type {} for root {}", root.r#type, root.name);
        }
    }

    Ok(())
}

fn validate_field(
    field: &KotlinField,
    type_names: &HashSet<&str>,
    allow_unknown_gc_types: bool,
    context: impl std::fmt::Display,
    target_kind: &str,
) -> Result<()> {
    match field.kind {
        KotlinFieldKind::Ref => {
            let Some(target) = field.r#type.as_deref() else {
                bail!("{context} is a ref but is missing a target type");
            };

            if !allow_unknown_gc_types && !type_names.contains(target) {
                bail!("{context} references unknown {target_kind}: {target}");
            }
        }
        _ => {
            if field.r#type.is_some() {
                bail!("{context} must not define a target type");
            }
        }
    }

    Ok(())
}
