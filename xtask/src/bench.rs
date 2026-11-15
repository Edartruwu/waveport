//! `cargo xtask bench-ir`: instruction-count benchmarks against a base revision.
//!
//! Instruction counts are deterministic enough to gate on, but only when both
//! sides are measured on the same machine. The base revision is therefore
//! checked out into a scratch worktree and measured first, and the working
//! tree is then compared with it.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::args::Args;
use crate::error::{Context, Error, Result};
use crate::git;
use crate::meta::{self, LoadOptions, Workspace};

/// Exit code the benchmark runner uses for "slower than the limit allows".
const REGRESSED: i32 = 3;

/// How far the instruction count of a benchmark may grow.
const LIMIT: &str = "--callgrind-limits=ir=2%";

/// Every instruction-count benchmark: the bench targets named `ir_*`.
const BENCH: &[&str] = &[
    "bench",
    "-p",
    "waveport-benches",
    "--bench",
    "ir_*",
    "--locked",
    "--",
];

/// Whether the tools the instruction-count benchmarks need are installed.
pub(crate) fn tools_installed() -> bool {
    ["valgrind", "gungraun-runner"].iter().all(|tool| {
        Command::new(tool)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Runs the benchmarks, comparing with `--base <rev>` when given.
pub(crate) fn run(args: &[String]) -> Result<bool> {
    let mut args = Args::new(args);
    let base = args.value("--base")?;
    args.finish()?;
    if !tools_installed() {
        return Err(Error::new(
            "instruction-count benchmarks need `valgrind` and `gungraun-runner` (Linux only)",
        ));
    }
    let workspace = meta::load(&LoadOptions::default())?;
    compare(&workspace, base.as_deref())
}

/// Measures the working tree, against `base` when that revision has benchmarks.
pub(crate) fn compare(workspace: &Workspace, base: Option<&str>) -> Result<bool> {
    let with_baseline = match base {
        Some(rev) if git::exists_at(&workspace.root, rev, "benches/Cargo.toml")? => {
            record_baseline(workspace, rev)?;
            true
        }
        Some(rev) => {
            println!("bench-ir: {rev} has no benchmarks; measuring without a baseline");
            false
        }
        None => false,
    };
    let mut command = meta::cargo();
    command
        .current_dir(&workspace.root)
        // The same directory the baseline was saved under.
        .env("CARGO_TARGET_DIR", &workspace.target_dir)
        .args(BENCH);
    if with_baseline {
        command.args(["--baseline=base", LIMIT]);
    }
    let status = command.status().context("could not run `cargo bench`")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(REGRESSED) => {
            eprintln!("error[BENCH001]: instruction counts grew beyond the limit ({LIMIT})");
            Ok(false)
        }
        _ => Err(Error::new("the instruction-count benchmarks could not run")),
    }
}

/// Measures `rev` in a scratch worktree and saves the result as the baseline.
fn record_baseline(workspace: &Workspace, rev: &str) -> Result<()> {
    let dir = workspace.target_dir.join("xtask").join("bench-base");
    let dir_text = dir.to_string_lossy().into_owned();
    remove_worktree(&workspace.root, &dir_text);
    git::run(
        &workspace.root,
        &["worktree", "add", "--detach", "--force", &dir_text, rev],
    )?;
    let status = meta::cargo()
        .current_dir(&dir)
        .env("CARGO_TARGET_DIR", &workspace.target_dir)
        .args(BENCH)
        .arg("--save-baseline=base")
        .status();
    remove_worktree(&workspace.root, &dir_text);
    if status
        .context("could not run `cargo bench` on the base revision")?
        .success()
    {
        Ok(())
    } else {
        Err(Error::new(format!(
            "the benchmarks failed on base revision {rev}"
        )))
    }
}

/// Removes the scratch worktree if it exists; a missing one is not an error.
fn remove_worktree(root: &Path, dir: &str) {
    let _ = Command::new("git")
        .current_dir(root)
        .args(["worktree", "remove", "--force", dir])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
