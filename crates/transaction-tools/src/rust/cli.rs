use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::common::{create_parent_dir, read_bytes, write_bytes, write_json};

#[derive(Parser)]
#[command(name = "twasm-rust")]
#[command(about = "Inspect and rewrite Rust transactional Wasm metadata")]
pub struct Cli {
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
    Build {
        #[arg(long)]
        manifest_path: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        release: bool,
        #[arg(long)]
        report: Option<PathBuf>,
    },
}

pub fn main() -> Result<()> {
    let mut stdout = std::io::stdout();
    run(Cli::parse(), &mut stdout)
}

pub fn run(cli: Cli, stdout: &mut impl Write) -> Result<()> {
    match cli.command {
        Command::Inspect { input } => {
            let bytes = read_bytes(&input)?;
            let report = super::metadata::inspect_module(&bytes)?;
            writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)?;
        }
        Command::Rewrite {
            input,
            output,
            report,
        } => {
            let bytes = read_bytes(&input)?;
            let (rewritten, rewrite_report) = super::rewrite::rewrite_module(&bytes)?;
            write_bytes(&output, rewritten)?;

            if let Some(report_path) = report {
                write_json(&report_path, &rewrite_report)?;
            }
        }
        Command::Build {
            manifest_path,
            output,
            release,
            report,
        } => {
            build_and_rewrite(manifest_path, output, release, report)?;
        }
    }

    Ok(())
}

fn build_and_rewrite(
    manifest_path: PathBuf,
    output: PathBuf,
    release: bool,
    report: Option<PathBuf>,
) -> Result<()> {
    let mut command = ProcessCommand::new("cargo");
    command
        .arg("build")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--target-dir")
        .arg("target")
        .arg("--manifest-path")
        .arg(&manifest_path);
    if release {
        command.arg("--release");
    }

    let status = command
        .status()
        .with_context(|| "failed to run cargo build for transactional Rust guest")?;
    if !status.success() {
        bail!("cargo build failed for {}", manifest_path.display());
    }

    let package_name = package_name_from_manifest(&manifest_path)?;
    let profile = if release { "release" } else { "debug" };
    let wasm_path = PathBuf::from("target")
        .join("wasm32-unknown-unknown")
        .join(profile)
        .join(format!("{}.wasm", wasm_artifact_stem(&package_name)));
    let bytes = read_built_wasm(&wasm_path)?;
    let (rewritten, rewrite_report) = super::rewrite::rewrite_module(&bytes)?;

    create_parent_dir(&output)?;
    write_bytes(&output, rewritten)?;

    if let Some(report_path) = report {
        create_parent_dir(&report_path)?;
        write_json(&report_path, &rewrite_report)?;
    }

    Ok(())
}

fn package_name_from_manifest(manifest_path: &Path) -> Result<String> {
    let manifest = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: toml::Value = toml::from_str(&manifest)
        .with_context(|| format!("failed to parse {}", manifest_path.display()))?;
    manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        .map(str::to_owned)
        .with_context(|| format!("missing [package].name in {}", manifest_path.display()))
}

fn wasm_artifact_stem(package_name: &str) -> String {
    package_name.replace('-', "_")
}

fn read_built_wasm(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to read built wasm {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{package_name_from_manifest, wasm_artifact_stem};

    #[test]
    fn package_name_reads_manifest_document() {
        let tempdir = tempfile::tempdir().unwrap();
        let manifest = tempdir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"
                [package]
                name = "transaction-rust-bank"
                version = "0.0.0"

                [workspace]
            "#,
        )
        .unwrap();

        assert_eq!(
            package_name_from_manifest(&manifest).unwrap(),
            "transaction-rust-bank"
        );
        assert_eq!(
            wasm_artifact_stem("transaction-rust-bank"),
            "transaction_rust_bank"
        );
    }
}
