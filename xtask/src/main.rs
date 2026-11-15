//! Waveport repository gates.
//!
//! Stability: internal
//!
//! Run a command with `cargo xtask <command>`; `cargo xtask help` lists them.
//! Exit codes: 0 when the gate passes, 1 when it finds violations, 2 when the
//! tool itself could not run.

mod arch;
mod args;
mod assets;
mod bench;
mod ci;
mod error;
mod git;
mod lint_sync;
mod meta;
mod report;
mod semver;
mod tier;
mod title;

use std::process::ExitCode;

use crate::error::{Error, Result};

const HELP: &str = "\
Usage: cargo xtask <command> [options]

Gates:
  arch-check     Check the dependency rule between layers
  tier-check     Check the stability tiers
  lint-sync      Check that lint configuration has not drifted
  check-title    Check a pull-request title: <title> | --env <VARIABLE>
  semver         Check for breaking API changes:
                 --self-test | --baseline-rev <rev> [--title <title> | --title-env <VARIABLE>]
  bench-ir       Run instruction-count benchmarks: [--base <rev>]

Setup:
  fetch-assets   Download the model files pinned in assets/assets.lock.toml
                 into target/assets: [--check] verifies without downloading

Everything at once:
  ci             Run every gate, cheapest first
                 [--fast] [--only <gate>] [--keep-going] [--strict]

  help           Show this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[String]) -> Result<bool> {
    let Some((command, rest)) = args.split_first() else {
        return Err(Error::new(format!("no command given\n\n{HELP}")));
    };
    match command.as_str() {
        "arch-check" => arch::run(rest),
        "tier-check" => tier::run(rest),
        "lint-sync" => lint_sync::run(rest),
        "check-title" => title::run(rest),
        "semver" => semver::run(rest),
        "bench-ir" => bench::run(rest),
        "fetch-assets" => assets::run(rest),
        "ci" => ci::run(rest),
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(true)
        }
        other => Err(Error::new(format!(
            "unknown command `{other}`; try `cargo xtask help`"
        ))),
    }
}
