use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

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
            let bytes =
                fs::read(&input).with_context(|| format!("failed to read {}", input.display()))?;
            let report = crate::metadata::inspect_module(&bytes)?;
            writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)?;
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
    let bytes = fs::read(&wasm_path)
        .with_context(|| format!("failed to read built wasm {}", wasm_path.display()))?;
    let (rewritten, rewrite_report) = crate::rewrite::rewrite_module(&bytes)?;

    create_parent_dir(&output)?;
    fs::write(&output, rewritten)
        .with_context(|| format!("failed to write {}", output.display()))?;

    if let Some(report_path) = report {
        create_parent_dir(&report_path)?;
        fs::write(&report_path, serde_json::to_vec_pretty(&rewrite_report)?)
            .with_context(|| format!("failed to write {}", report_path.display()))?;
    }

    Ok(())
}

fn package_name_from_manifest(manifest_path: &Path) -> Result<String> {
    let manifest = fs::read_to_string(manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: toml::Value = manifest
        .parse()
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

fn create_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))
}
