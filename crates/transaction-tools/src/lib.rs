pub mod cli;
pub mod kotlin_cli;
pub mod kotlin_inspect;
pub mod kotlin_metadata;
pub mod kotlin_rewrite;
pub mod metadata;
pub mod rewrite;

pub use kotlin_inspect::{KotlinInspectReport, inspect_sidecar};
pub use kotlin_metadata::{KotlinSidecar, parse_kotlin_sidecar};
pub use kotlin_rewrite::{KotlinRewriteReport, rewrite_kotlin_module};
pub use metadata::{MetadataReport, inspect_module};
pub use rewrite::{RewriteReport, rewrite_module};
