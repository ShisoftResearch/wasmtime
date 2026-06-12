use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "twasm-rust")]
#[command(about = "Inspect and rewrite Rust transactional Wasm metadata")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Inspect {
        input: PathBuf,
    },
    Rewrite {
        input: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

pub fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect { input } => {
            let bytes =
                fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let report = crate::metadata::inspect_module(&bytes)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Rewrite {
            input,
            output,
            report,
        } => {
            let bytes =
                fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let (rewritten, rewrite_report) = crate::rewrite::rewrite_module(&bytes)?;
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
