use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::common::{read_bytes, write_bytes, write_json};

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
            let bytes = read_bytes(&metadata)?;
            let sidecar = super::metadata::parse_kotlin_sidecar(&bytes[..])
                .with_context(|| format!("failed to parse {}", metadata.display()))?;
            let report = super::inspect::inspect_sidecar(&sidecar);
            if let Some(wasm) = wasm {
                let wasm_bytes = read_bytes(&wasm)?;
                let wasm_shape = super::inspect::inspect_wasm_shape(&wasm_bytes)
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
            let input_bytes = read_bytes(&input)?;
            let metadata_bytes = read_bytes(&metadata)?;
            let sidecar = super::metadata::parse_kotlin_sidecar(&metadata_bytes[..])
                .with_context(|| format!("failed to parse {}", metadata.display()))?;
            let (rewritten, rewrite_report) =
                super::rewrite::rewrite_kotlin_module(&input_bytes, &sidecar)
                    .with_context(|| format!("failed to rewrite {}", input.display()))?;
            write_bytes(&output, rewritten)?;
            if let Some(report_path) = report {
                write_json(&report_path, &rewrite_report)?;
            }
        }
    }

    Ok(())
}
