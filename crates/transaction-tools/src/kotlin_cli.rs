use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "twasm-kotlin")]
#[command(about = "Build, inspect, and lower Kotlin/Wasm transactional object modules")]
pub struct KotlinCli {
    #[command(subcommand)]
    command: KotlinCommand,
}

#[derive(Subcommand)]
enum KotlinCommand {
    Inspect {
        #[arg(long = "metadata")]
        metadata: PathBuf,
        #[arg(long = "wasm")]
        wasm: Option<PathBuf>,
    },
    Rewrite {
        input: PathBuf,
        #[arg(long = "metadata")]
        metadata: PathBuf,
        #[arg(long = "output")]
        output: PathBuf,
        #[arg(long = "report")]
        report: Option<PathBuf>,
    },
}

pub fn main() -> Result<()> {
    let mut stdout = std::io::stdout();
    run(KotlinCli::parse(), &mut stdout)
}

pub fn run(cli: KotlinCli, stdout: &mut impl Write) -> Result<()> {
    match cli.command {
        KotlinCommand::Inspect { metadata, wasm } => {
            let bytes = fs::read(&metadata)
                .with_context(|| format!("failed to read {}", metadata.display()))?;
            let sidecar = crate::kotlin_metadata::parse_kotlin_sidecar(&bytes[..])
                .with_context(|| format!("failed to parse {}", metadata.display()))?;
            let report = crate::kotlin_inspect::inspect_sidecar(&sidecar);
            if let Some(wasm) = wasm {
                let wasm_bytes = fs::read(&wasm)
                    .with_context(|| format!("failed to read {}", wasm.display()))?;
                let wasm_shape = crate::kotlin_inspect::inspect_wasm_shape(&wasm_bytes)
                    .with_context(|| format!("failed to inspect {}", wasm.display()))?;
                let mut value = serde_json::to_value(&report)?;
                value["wasm_shape"] = serde_json::to_value(&wasm_shape)?;
                writeln!(stdout, "{}", serde_json::to_string_pretty(&value)?)?;
            } else {
                writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)?;
            }
        }
        KotlinCommand::Rewrite {
            input,
            metadata,
            output,
            report,
        } => {
            let input_bytes =
                fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let metadata_bytes = fs::read(&metadata)
                .with_context(|| format!("failed to read {}", metadata.display()))?;
            let sidecar = crate::kotlin_metadata::parse_kotlin_sidecar(&metadata_bytes[..])
                .with_context(|| format!("failed to parse {}", metadata.display()))?;
            let (rewritten, rewrite_report) =
                crate::kotlin_rewrite::rewrite_kotlin_module(&input_bytes, &sidecar)
                    .with_context(|| format!("failed to rewrite {}", input.display()))?;
            fs::write(&output, rewritten)
                .with_context(|| format!("failed to write {}", output.display()))?;
            if let Some(report_path) = report {
                fs::write(&report_path, serde_json::to_vec_pretty(&rewrite_report)?)
                    .with_context(|| format!("failed to write {}", report_path.display()))?;
            }
        }
    }

    Ok(())
}
