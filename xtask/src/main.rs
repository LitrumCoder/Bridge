#![allow(clippy::print_stdout, clippy::print_stderr)]

mod bench;
mod deps;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  check-deps            fail if pure crates depend on OS, async or network crates
  bench-check [--strict]  compare criterion estimates with benches/targets.toml";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("check-deps") => deps::run(&workspace_root()),
        Some("bench-check") => bench::run(&workspace_root(), args.iter().any(|a| a == "--strict")),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}
