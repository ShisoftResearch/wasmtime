use clap::Parser;
use wasmtime_transaction_tools::cli::Cli;

#[test]
fn build_subcommand_accepts_package_and_output() {
    Cli::parse_from([
        "twasm-rust",
        "build",
        "--manifest-path",
        "examples/transaction-rust/bank/Cargo.toml",
        "--output",
        "target/transaction-rust/bank.twasm.wasm",
    ]);
}
