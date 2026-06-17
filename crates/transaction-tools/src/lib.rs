pub mod common;
pub mod kotlin;
pub mod rust;

pub mod cli {
    pub use crate::rust::cli::*;
}

pub mod kotlin_cli {
    pub use crate::kotlin::cli::*;
}

pub mod kotlin_inspect {
    pub use crate::kotlin::inspect::*;
}

pub mod kotlin_metadata {
    pub use crate::kotlin::metadata::*;
}

pub mod kotlin_rewrite {
    pub use crate::kotlin::rewrite::*;
}

pub mod metadata {
    pub use crate::rust::metadata::*;
}

pub mod rewrite {
    pub use crate::rust::rewrite::*;
}

pub use kotlin::inspect::{KotlinInspectReport, inspect_sidecar};
pub use kotlin::metadata::{KotlinSidecar, parse_kotlin_sidecar};
pub use kotlin::rewrite::{KotlinRewriteReport, rewrite_kotlin_module};
pub use rust::metadata::{MetadataReport, inspect_module};
pub use rust::rewrite::{RewriteReport, rewrite_module};
